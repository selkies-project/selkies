//! The frame pacing every capture backend shares: when a display is due a frame, and how far
//! a change that just happened may bring the next one forward.
//!
//! A capture renders on its frame timer, and something that actually happened -- input the
//! compositor processed, a frame a host delivered, a region the X server reports damaged --
//! may render one ahead of that timer. Input's pull spends from a budget that refills slowly,
//! so input faster than the cadence raises the output rate by no more than the refill; a
//! change the screen or a host reports spends only what its own late frames paid back, so it
//! moves the cadence onto the frames it reports without raising the rate. Either way a change
//! arriving on its own schedule is published as it lands rather than up to a period later.
//!
//! The X11 captures, which the X server's damage reports wake, pace in windows instead
//! (`FramePace::windowed`): one frame a period, a change taking its period's frame as it lands,
//! as the NvFBC driver pushes frames, and a change landing in a period a frame already took
//! sent as an extra frame from a budget that refills slowly, so a source drawing at the cadence
//! keeps its own periods.

use std::time::{Duration, Instant};

/// What asks a display for a frame: its frame timer, fresh input, a frame the host just
/// delivered under host capture, or a region the X server reports damaged. All but the timer
/// say something has actually changed, and so may pull the cadence's phase forward.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TickTrigger {
    Timer,
    Input,
    HostFrame,
    Damage,
}

impl TickTrigger {
    /// Whether this trigger may render ahead of the timer, moving the cadence to its phase.
    fn pulls_forward(self) -> bool {
        !matches!(self, TickTrigger::Timer)
    }

    /// Whether the time this trigger may pull a frame forward by refills with wall time: fresh
    /// input's, whose latency the pull is for. A screen that changes faster than the cadence, a
    /// video or an application rendering unsynced, would otherwise raise the output rate by the
    /// refill for as long as it runs.
    fn refills(self) -> bool {
        matches!(self, TickTrigger::Input)
    }
}

/// The share of a frame period the timer's tick may run early by, absorbing its wakeup jitter.
const TIMER_TICK_MIN_FRACTION: f64 = 0.9;
/// The share of a frame period that has to pass before input or a host frame may pull the
/// next frame forward.
const INPUT_TICK_MIN_FRACTION: f64 = 0.5;
/// The share of wall time that accrues as time an input pull may bring frames forward by. A
/// frame pulled forward by some time is one the cadence earns that much later, so the
/// sustained rate can rise above the configured one by at most this share.
const INPUT_BORROW_REFILL: f64 = 1.0 / 32.0;
/// The share of a frame period the timer waits beyond a whole one after an input frame while
/// the input paces itself at the cadence, so input arriving a little slower than the timer
/// still renders its own frames instead of landing just behind a timer frame and waiting a
/// period.
const TIMER_GRACE_AFTER_PULL: f64 = 0.25;
/// The share of a frame period before the next window opens in which a change, paced in windows,
/// takes that window's frame at once rather than waiting for it to open. One frame a window
/// still holds the rate at the cadence; a source drawing at the cadence whose frames end just
/// short of the boundary otherwise waits for it every frame (a 60 fps full-screen animation on
/// Xvfb waited 0.8 ms a frame on average, in some phases of its clock against the windows').
const WINDOW_EARLY_FRACTION: f64 = 0.25;
/// The share of a period after the last frame from which a change landing in a window a frame
/// already took may be sent as an extra frame (`FramePace::windowed`).
const EXTRA_MIN_FRACTION: f64 = 0.5;
/// The extra frames a capture paced in windows holds in reserve.
const EXTRA_CAP: f64 = 2.0;
/// The share of an extra frame each period adds to that reserve: the sustained rate rises
/// above the configured one by at most this share. A slower refill leaves a source at the
/// cadence locked behind a keystroke's extra frame for longer (a 60 Hz animation with a
/// keystroke every 0.7 s waited 7.6 ms a frame at the median at 1/64, 12.5 ms at 1/128).
const EXTRA_REFILL: f64 = 1.0 / 32.0;
/// The periods without a reported change after which the screen counts as still
/// (`FramePace::still`): more than the timer waits after a change's frame.
const STILL_PERIODS: f64 = 1.5;
/// The input spacings, as shares of the frame period, that count as pacing at the cadence:
/// a client at the cadence give or take its jitter. A faster client is borrowing frames and a
/// slower one is content the timer paces, and the timer keeps its period behind either.
const PACED_INPUT_MIN_FRACTION: f64 = 0.75;
const PACED_INPUT_MAX_FRACTION: f64 = 1.25;

/// A capture's frame pacing: the last frame it rendered, and how far a pull may still bring
/// the next one forward.
///
/// The shared frame timer fires for the earliest due capture; each capture renders on it once
/// its own period has (nearly) passed. Fresh input may also render a frame, and so may a frame
/// the host compositor delivers under host capture or a region the X server reports damaged:
/// a whole period after the last one the pull renders at once, and from half a period on it
/// brings the frame forward, spending the time it comes early by from a budget that is paid
/// back by a pull that arrives late and holds half a period at most. Input's budget also
/// refills at `INPUT_BORROW_REFILL`; the others' does not (`TickTrigger::refills`). A pointer
/// moving at the client's refresh rate is thus captured as it lands rather than up to a period
/// later, and a host's or an application's frames are published as they arrive rather than on
/// the next timer tick, while input at a rate the cadence cannot follow raises the output rate
/// by no more than the refill, and frames reported faster than the cadence not at all.
///
/// After an input frame, while the input's own spacing sits at the cadence
/// (`PACED_INPUT_MIN_FRACTION` to `PACED_INPUT_MAX_FRACTION` of a period), the timer stands
/// back for `TIMER_GRACE_AFTER_PULL` beyond a period, so such input keeps rendering its own
/// frames whether it runs a hair faster or slower than the timer, or jitters around it: a timer
/// that resumed exactly a period after the frame would render just ahead of a move landing a
/// little late, which then could not pull and waited a period. When the input stops, the timer
/// renders after the grace and carries on from there. A faster client, a slower one, and a
/// host's frames leave the timer its period as before.
///
/// Damage the X server reports at the cadence earns the same grace: it is an application
/// presenting on the server's vblank, which runs on a clock of its own (a framebuffer server's
/// fake vblank fires on a millisecond timer, so its frames land up to a millisecond either side
/// of the period). Without the grace a timer tick taken just ahead of such a frame captures the
/// one before it again, and the frame it missed is published late or never.
#[derive(Debug, Default)]
pub struct FramePace {
    /// Last tick this capture actually rendered.
    pub last_tick: Option<Instant>,
    /// Whether that frame was input's, with the input pacing itself at the cadence, so the
    /// timer stands back for the next move.
    last_paced: bool,
    /// Input's budget left when it was last spent, and when that was; a fresh capture holds the
    /// cap.
    borrow_budget: Option<(Duration, Instant)>,
    /// The budget host frames and damage pull from, which refills only from their own late
    /// frames; a fresh capture holds the cap.
    phase_budget: Option<Duration>,
    /// Where the timer was held off to by a tick that rendered nothing (host capture with no
    /// fresh frame), so it neither spins nor counts against the next frame.
    deferred_until: Option<Instant>,
    /// The last damage frame, and the running average of the spacing between damage frames.
    last_damage: Option<Instant>,
    /// A still tick gave the next pull a fresh capture's budget, which that pull spends whole.
    rephase: bool,
    damage_spacing: Option<Duration>,
    /// Paced in windows (`Self::windowed`): the end of the last period a frame closed.
    window_end: Option<Instant>,
    windowed: bool,
    /// Paced in windows, the extra frames left when one was last sent, and when; a fresh
    /// capture holds the cap.
    extra: Option<(f64, Instant)>,
}

impl FramePace {
    /// Pacing in windows, for a capture a change wakes (XShm and DRI3 on the X server's damage
    /// reports, the compositor on a client's commit, input or a host frame): each
    /// period is a window holding one frame. A change takes its window's frame as it lands; a
    /// window no change took ends on the timer's frame. A change is published at once unless a
    /// change already took its window, in which case it waits for the next window, taking it
    /// `WINDOW_EARLY_FRACTION` of a period before it opens: the NvFBC capture's schedule, whose
    /// driver pushes frames the same way. Without windows a change could come forward no
    /// earlier than half a period after the last frame, and one landing just after a timer
    /// frame waited that long.
    ///
    /// Such a change is instead sent as an extra frame where the reserve holds one
    /// (`EXTRA_CAP`, refilled at `EXTRA_REFILL` a period), from `EXTRA_MIN_FRACTION` of a
    /// period after the last frame, and leaves the windows as they were. Taken from the next
    /// window, it would close that window ahead of a source drawing at the cadence, whose next
    /// frame then lands in a window already taken and waits as long again: any wait sustains
    /// itself, a source a hair faster than the cadence grows it to a period, and a 60 Hz
    /// application on its own clock under a 60 fps capture waited a median 6 ms a frame and up
    /// to 15, and one keystroke during a 60 Hz animation held each of its frames 12.5 ms.
    pub fn windowed() -> Self {
        Self {
            windowed: true,
            ..Self::default()
        }
    }

    /// Where the window a frame at `now` falls in ends: the open window after the last one a
    /// frame closed, or a later one where whole windows went by without a tick.
    fn window_closed_by(&self, period: Duration, now: Instant) -> Instant {
        match self.window_end {
            None => now,
            Some(end) => {
                let mut close = end + period;
                while close < now {
                    close += period;
                }
                close
            }
        }
    }

    /// A tick paced in windows: a timer tick ends the open window whether it sent a frame or
    /// not, which keeps the windows on the timer's schedule; a change's frame closes the window
    /// it lands in, and a change not sent leaves it open. A tick that came late (a busy capture
    /// or compositor thread, an encode that ran long) still ends the window it was due for, so
    /// the next opens on schedule and the rate holds: moving the grid to the late tick gave up
    /// the time it was late by, every time, and a loaded session streamed below its rate. A
    /// change taken less than a window after the open window's timer was due ends that window
    /// too, or a screen changing a little slower than the cadence lost a window to each encode
    /// that ran past a period. Only windows that passed whole without a tick are given up. A
    /// change sent before the next window would take it is an extra frame, which spends the
    /// reserve and leaves the windows as they were.
    fn window_tick(&mut self, trigger: TickTrigger, period: Duration, now: Instant, sent: bool) {
        let scheduled = self.next_due(period, now);
        if trigger == TickTrigger::Timer {
            self.window_end = Some(if now <= scheduled {
                scheduled
            } else {
                scheduled
                    + period * ((now - scheduled).as_nanos() / period.as_nanos().max(1)) as u32
            });
        } else if sent && now < self.window_pull(period, now) {
            let left = (self.extra_frames(period, now) - 1.0).max(0.0);
            self.extra = Some((left, now));
        } else if sent {
            self.window_end = Some(
                if self.window_end.is_some() && now > scheduled && now < scheduled + period {
                    scheduled
                } else {
                    self.window_closed_by(period, now)
                },
            );
        }
        if sent {
            if trigger == TickTrigger::Damage {
                self.damage_paced(period, now);
            }
            self.last_tick = Some(now);
            self.deferred_until = None;
        }
    }

    /// Whether a capture rendering every `period` is due a frame at `now`, given what asks.
    /// Paced in windows, a change is due once its window is open and the timer at the open
    /// window's end, give or take the timer's own lateness.
    pub fn due(&self, trigger: TickTrigger, period: Duration, now: Instant) -> bool {
        if self.windowed {
            return if trigger.pulls_forward() {
                now >= self.pull_at(trigger, period, now)
            } else {
                now + period.mul_f64(1.0 - TIMER_TICK_MIN_FRACTION) >= self.next_due(period, now)
            };
        }
        let Some(last) = self.last_tick else {
            return true;
        };
        let elapsed = now.saturating_duration_since(last);
        if !trigger.pulls_forward() {
            return elapsed >= self.timer_wait(period);
        }
        elapsed >= period
            || (elapsed >= period.mul_f64(INPUT_TICK_MIN_FRACTION)
                && self.budget(trigger, period, now) >= period - elapsed)
    }

    /// Whether input spaced `interval` apart paces itself at the cadence of `period`.
    pub fn input_paced(interval: Option<Duration>, period: Duration) -> bool {
        interval.is_some_and(|i| {
            i >= period.mul_f64(PACED_INPUT_MIN_FRACTION)
                && i <= period.mul_f64(PACED_INPUT_MAX_FRACTION)
        })
    }

    /// Record the frame rendered at `now`. A pull ahead of the period spends the budget by
    /// what it came early; one behind the period pays back what it came late. A timer tick
    /// within half a period of when it was due counts as on time, so a timer that wakes a
    /// little late, as one does under load, keeps the rate instead of losing what it was late
    /// by every frame. `input_paced` says whether the input driving the capture paces itself
    /// at the cadence (`Self::input_paced`), which is what lets the timer stand back after an
    /// input frame.
    pub fn ticked(
        &mut self,
        trigger: TickTrigger,
        period: Duration,
        now: Instant,
        input_paced: bool,
    ) {
        if self.windowed {
            self.window_tick(trigger, period, now, true);
            return;
        }
        if trigger.pulls_forward()
            && let Some(last) = self.last_tick
        {
            let elapsed = now.saturating_duration_since(last);
            let budget = self.budget(trigger, period, now);
            let left = if elapsed < period {
                budget.saturating_sub(period - elapsed)
            } else {
                (budget + (elapsed - period)).min(period.mul_f64(INPUT_TICK_MIN_FRACTION))
            };
            if trigger.refills() {
                self.borrow_budget = Some((left, now));
            } else if std::mem::take(&mut self.rephase) {
                self.phase_budget = Some(Duration::ZERO);
            } else {
                self.phase_budget = Some(left);
            }
        }
        let mut at = now;
        if trigger == TickTrigger::Timer && self.last_tick.is_some() {
            let scheduled = self.next_due(period, now);
            let off = if now > scheduled {
                now - scheduled
            } else {
                scheduled - now
            };
            if off < period / 2 {
                at = scheduled;
            }
        }
        let damage_paced = trigger == TickTrigger::Damage && self.damage_paced(period, now);
        self.last_paced = (trigger == TickTrigger::Input && input_paced) || damage_paced;
        self.last_tick = Some(at);
        self.deferred_until = None;
    }

    /// Fold a damage frame at `now` into the spacing of damage frames, and say whether that
    /// spacing sits at the cadence of `period`.
    fn damage_paced(&mut self, period: Duration, now: Instant) -> bool {
        if let Some(prev) = self.last_damage {
            let dt = now.saturating_duration_since(prev).min(period * 4);
            self.damage_spacing = Some(
                self.damage_spacing
                    .map_or(dt, |avg| avg.mul_f64(0.875) + dt.mul_f64(0.125)),
            );
        }
        self.last_damage = Some(now);
        Self::input_paced(self.damage_spacing, period)
    }

    /// How long after the last frame the timer's own tick renders again.
    fn timer_wait(&self, period: Duration) -> Duration {
        if self.last_paced {
            period.mul_f64(1.0 + TIMER_GRACE_AFTER_PULL)
        } else {
            period.mul_f64(TIMER_TICK_MIN_FRACTION)
        }
    }

    /// Whether the screen stands still at `now`: no change reported for `STILL_PERIODS` periods,
    /// longer than the timer waits after a change's frame, so a screen that keeps changing never
    /// does.
    pub fn still(&self, period: Duration, now: Instant) -> bool {
        self.last_damage
            .is_none_or(|at| now.saturating_duration_since(at) >= period.mul_f64(STILL_PERIODS))
    }

    /// Give the next change the pull a fresh capture has, for a tick that found the screen
    /// still (`Self::still`): it moves the cadence onto the change and leaves nothing to
    /// borrow, so the frame after it waits a whole period.
    pub fn stood_still(&mut self) {
        self.phase_budget = None;
        self.rephase = true;
    }

    /// Record a tick of a capture the Damage extension wakes (the X11 paths), once it is known
    /// whether it sent a frame. Paced in windows (`Self::windowed`), a timer tick ends the open
    /// window and a change's frame closes the one it lands in. Otherwise a sent frame counts
    /// (`Self::ticked`), one that published nothing holds the timer off a period
    /// (`Self::defer`), and a sent timer's tick that finds the screen still gives the change
    /// after it a fresh capture's pull (`Self::stood_still`): where every tick is sent
    /// (streaming mode, XShm), the changes before it spent the budget, which a still screen's
    /// frames, never late, do not pay back, and each change would wait for the next tick. A
    /// tick that sent nothing spent nothing, and the budget refills alone.
    pub fn record_tick(
        &mut self,
        trigger: TickTrigger,
        period: Duration,
        now: Instant,
        sent: bool,
    ) {
        if self.windowed {
            self.window_tick(trigger, period, now, sent);
            return;
        }
        if !sent {
            self.defer(now + period);
            return;
        }
        self.ticked(trigger, period, now, false);
        if trigger == TickTrigger::Timer && self.still(period, now) {
            self.stood_still();
        }
    }

    /// Hold the timer off this capture until `until` without a frame counting: a tick that
    /// published nothing must neither spin the timer nor move the cadence a fresh frame is
    /// measured against.
    pub fn defer(&mut self, until: Instant) {
        self.deferred_until = Some(until);
    }

    /// Time since this capture last rendered, if it has.
    pub fn since_last_tick(&self, now: Instant) -> Option<Duration> {
        self.last_tick
            .map(|last| now.saturating_duration_since(last))
    }

    /// How far a pull by `trigger` may bring the next frame forward at `now`.
    fn budget(&self, trigger: TickTrigger, period: Duration, now: Instant) -> Duration {
        let cap = period.mul_f64(INPUT_TICK_MIN_FRACTION);
        if !trigger.refills() {
            return self.phase_budget.unwrap_or(cap);
        }
        match self.borrow_budget {
            None => cap,
            Some((left, at)) => (left
                + now
                    .saturating_duration_since(at)
                    .mul_f64(INPUT_BORROW_REFILL))
            .min(cap),
        }
    }

    /// The earliest `trigger`, one that pulls forward, may render: a whole period after the
    /// last frame, less what its budget still allows it to borrow. A caller that waits until
    /// then need not test `due` again, since a budget only grows with time.
    pub fn pull_at(&self, trigger: TickTrigger, period: Duration, now: Instant) -> Instant {
        if self.windowed {
            let window = self.window_pull(period, now);
            return match self.last_tick {
                Some(last) if self.extra_frames(period, now) >= 1.0 => {
                    window.min(last + period.mul_f64(EXTRA_MIN_FRACTION))
                }
                _ => window,
            };
        }
        self.last_tick.map_or(now, |last| {
            last + period.saturating_sub(self.budget(trigger, period, now))
        })
    }

    /// Paced in windows, when a change may take the window after the one the last frame
    /// closed: from `WINDOW_EARLY_FRACTION` of a period before it opens.
    fn window_pull(&self, period: Duration, now: Instant) -> Instant {
        self.window_end
            .map_or(now, |end| end - period.mul_f64(WINDOW_EARLY_FRACTION))
    }

    /// The extra frames a capture paced in windows may send at `now`.
    fn extra_frames(&self, period: Duration, now: Instant) -> f64 {
        self.extra.map_or(EXTRA_CAP, |(left, at)| {
            let periods = now.saturating_duration_since(at).as_secs_f64() / period.as_secs_f64();
            (left + periods * EXTRA_REFILL).min(EXTRA_CAP)
        })
    }

    /// When the timer is next due for this capture: a period after the last frame, and the
    /// grace beyond that after an input frame the input paced.
    pub fn next_due(&self, period: Duration, now: Instant) -> Instant {
        if self.windowed {
            // The end of the open window, where no change took it, unless a tick that published
            // nothing held the timer off (`Self::defer`).
            let due = self.window_end.map_or(now, |end| end + period);
            return self.deferred_until.map_or(due, |held| held.max(due));
        }
        let due = self.last_tick.map_or(now, |last| {
            last + if self.last_paced {
                period.mul_f64(1.0 + TIMER_GRACE_AFTER_PULL)
            } else {
                period
            }
        });
        self.deferred_until.map_or(due, |held| held.max(due))
    }

    /// Paced in windows, when the window `now` falls in opened: the boundary a display's vblank
    /// would fall on. None before the first frame.
    pub fn window_opened(&self, period: Duration, now: Instant) -> Option<Instant> {
        let end = self.window_end?;
        let p = period.as_nanos().max(1);
        Some(if now >= end {
            end + period * ((now - end).as_nanos() / p) as u32
        } else {
            end - period * (end - now).as_nanos().div_ceil(p) as u32
        })
    }

    /// When a frame timer armed at `now` next has to fire for this capture. It wakes for a window
    /// still to open whether or not a change waits on it yet: a change landing later has nothing
    /// else to wake it, since a client that draws on frame callbacks goes quiet after its commit.
    /// A window already open takes a change as it lands, so only a change that could not be
    /// rendered keeps the timer close.
    pub fn next_wake(&self, period: Duration, now: Instant, change_pending: bool) -> Instant {
        let due = self.next_due(period, now);
        let pull = self.pull_at(TickTrigger::Damage, period, now);
        if change_pending || pull > now {
            due.min(pull)
        } else {
            due
        }
    }
}

#[cfg(test)]
mod pacing_tests {
    use super::*;

    const PERIOD: Duration = Duration::from_millis(20);

    fn at(base: Instant, ms: u64) -> Instant {
        base + Duration::from_millis(ms)
    }

    /// Paced in windows, a change is taken as it lands wherever it falls after a timer frame,
    /// and the timer frame its window would have ended on is skipped: the rate stays the
    /// cadence's.
    #[test]
    fn a_windowed_change_is_taken_as_it_lands_and_the_rate_holds() {
        let base = Instant::now();
        let mut pace = FramePace::windowed();
        pace.record_tick(TickTrigger::Timer, PERIOD, base, true);
        let mut frames = 1u32;
        let mut t = base;
        let end = base + Duration::from_secs(4);
        let mut i = 0u64;
        while t < end {
            // A change once every 5 periods, at a phase that walks through the window.
            let change = t + PERIOD * 4 + Duration::from_micros(300 + (i * 3700) % 19_500);
            i += 1;
            loop {
                let due = pace.next_due(PERIOD, t);
                if due >= change {
                    break;
                }
                t = due;
                pace.record_tick(TickTrigger::Timer, PERIOD, t, true);
                frames += 1;
            }
            let at = pace
                .pull_at(TickTrigger::Damage, PERIOD, change)
                .max(change);
            assert_eq!(at, change, "change {i} waited {:?}", at - change);
            pace.record_tick(TickTrigger::Damage, PERIOD, at, true);
            frames += 1;
            t = at;
        }
        let windows = (t - base).as_secs_f64() / PERIOD.as_secs_f64();
        assert!(
            f64::from(frames) <= windows.ceil() + 1.0,
            "{frames} frames in {windows:.1} windows"
        );
    }

    /// A second change in a window a change already took is sent as an extra frame half a
    /// period after the last, leaving the windows as they were, and once the reserve is spent
    /// waits for the next window, taken `WINDOW_EARLY_FRACTION` of a period before it opens.
    #[test]
    fn a_second_change_in_a_window_spends_the_reserve_then_waits_for_the_next() {
        let base = Instant::now();
        let mut pace = FramePace::windowed();
        pace.record_tick(TickTrigger::Timer, PERIOD, base, true);
        pace.record_tick(TickTrigger::Damage, PERIOD, at(base, 3), true);
        assert_eq!(
            pace.pull_at(TickTrigger::Damage, PERIOD, at(base, 5)),
            at(base, 13)
        );
        assert_eq!(
            pace.next_due(PERIOD, at(base, 5)),
            at(base, 40),
            "its window's timer frame is skipped"
        );
        pace.record_tick(TickTrigger::Damage, PERIOD, at(base, 13), true);
        assert_eq!(
            pace.next_due(PERIOD, at(base, 14)),
            at(base, 40),
            "the extra frame leaves the windows"
        );
        assert_eq!(
            pace.pull_at(TickTrigger::Damage, PERIOD, at(base, 14)),
            at(base, 15)
        );
        pace.record_tick(TickTrigger::Damage, PERIOD, at(base, 15), true);
        pace.record_tick(TickTrigger::Damage, PERIOD, at(base, 25), true);
        assert_eq!(
            pace.pull_at(TickTrigger::Damage, PERIOD, at(base, 26)),
            at(base, 35),
            "the reserve is spent"
        );
    }

    /// A source drawing at the cadence keeps its own windows through a change landing between
    /// two of its frames: each of its frames is taken as it lands.
    #[test]
    fn a_source_at_the_cadence_keeps_its_windows_through_a_stray_change() {
        let base = Instant::now();
        let mut pace = FramePace::windowed();
        pace.record_tick(TickTrigger::Timer, PERIOD, base, true);
        for k in 0..20u64 {
            let frame = at(base, 2 + 20 * k);
            assert!(
                pace.pull_at(TickTrigger::Damage, PERIOD, frame) <= frame,
                "frame {k}"
            );
            pace.record_tick(TickTrigger::Damage, PERIOD, frame, true);
            if k == 5 {
                let stray = at(base, 2 + 20 * k + 12);
                assert!(pace.pull_at(TickTrigger::Damage, PERIOD, stray) <= stray);
                pace.record_tick(TickTrigger::Damage, PERIOD, stray, true);
            }
        }
    }

    /// Changes faster than the cadence are published one a window, and the extra frames the
    /// reserve and its refill allow.
    #[test]
    fn windowed_changes_faster_than_the_cadence_raise_its_rate_by_the_refill_at_most() {
        let base = Instant::now();
        let mut pace = FramePace::windowed();
        pace.record_tick(TickTrigger::Timer, PERIOD, base, true);
        let (mut frames, mut changes, mut pending) = (1u32, 0u32, false);
        let mut next_change = base + PERIOD.mul_f64(0.7);
        // Stepped every 0.1 ms: a change is pending until its window opens, else the timer ends it.
        for step in 1..=20_000u32 {
            let now = base + Duration::from_micros(100) * step;
            if now >= next_change {
                pending = true;
                changes += 1;
                next_change += PERIOD.mul_f64(0.7);
            }
            if pending && now >= pace.pull_at(TickTrigger::Damage, PERIOD, now) {
                pace.record_tick(TickTrigger::Damage, PERIOD, now, true);
                pending = false;
                frames += 1;
            } else if now >= pace.next_due(PERIOD, now) {
                pace.record_tick(TickTrigger::Timer, PERIOD, now, true);
                frames += 1;
            }
        }
        // The first frame's, one for each window closed, and the extra frames.
        let closed =
            (pace.window_end.expect("windows") - base).as_secs_f64() / PERIOD.as_secs_f64();
        let extra = EXTRA_CAP + closed * EXTRA_REFILL;
        assert!(changes > 130, "{changes} changes");
        assert!(
            f64::from(frames) >= closed.round() + 1.0
                && f64::from(frames) <= closed.round() + 1.0 + extra,
            "{frames} frames over {closed:.1} windows"
        );
    }

    /// With streaming off the still ticks send nothing, and still keep the windows on the
    /// timer's schedule: a change after a still spell is taken at once.
    #[test]
    fn unsent_ticks_keep_the_windows() {
        let base = Instant::now();
        let mut pace = FramePace::windowed();
        pace.record_tick(TickTrigger::Timer, PERIOD, base, true);
        for k in 1..=10u64 {
            pace.record_tick(TickTrigger::Timer, PERIOD, at(base, 20 * k), false);
        }
        assert_eq!(pace.next_due(PERIOD, at(base, 200)), at(base, 220));
        assert!(pace.due(TickTrigger::Damage, PERIOD, at(base, 207)));
        pace.record_tick(TickTrigger::Damage, PERIOD, at(base, 207), true);
        assert_eq!(pace.next_due(PERIOD, at(base, 207)), at(base, 240));
    }

    /// The compositor asks `due` rather than `pull_at`: paced in windows, a change is due as
    /// its window opens and not before, and the timer at the open window's end.
    #[test]
    fn windowed_due_follows_the_windows() {
        let base = Instant::now();
        let mut pace = FramePace::windowed();
        pace.ticked(TickTrigger::Timer, PERIOD, base, false);
        // The window after the timer's frame is open at once.
        assert!(pace.due(TickTrigger::Damage, PERIOD, at(base, 3)));
        assert!(pace.due(TickTrigger::Input, PERIOD, at(base, 3)));
        assert!(!pace.due(TickTrigger::Timer, PERIOD, at(base, 3)));
        // A change takes it; the next is an extra frame from half a period on, at 13 ms.
        pace.ticked(TickTrigger::Damage, PERIOD, at(base, 3), false);
        assert!(!pace.due(TickTrigger::Damage, PERIOD, at(base, 12)));
        assert_eq!(
            pace.pull_at(TickTrigger::Damage, PERIOD, at(base, 12)),
            at(base, 13)
        );
        assert!(pace.due(TickTrigger::Damage, PERIOD, at(base, 20)));
        // The timer skips the window the change took and ends the next one, at 40 ms, a
        // little early counting.
        assert!(!pace.due(TickTrigger::Timer, PERIOD, at(base, 20)));
        assert!(pace.due(TickTrigger::Timer, PERIOD, at(base, 39)));
    }

    /// A tick that published nothing holds the timer off in windows too (host capture with no
    /// fresh frame), and the next frame lifts the hold.
    #[test]
    fn windowed_timer_keeps_a_deferral() {
        let base = Instant::now();
        let mut pace = FramePace::windowed();
        pace.ticked(TickTrigger::Timer, PERIOD, base, false);
        pace.defer(at(base, 70));
        assert_eq!(pace.next_due(PERIOD, at(base, 21)), at(base, 70));
        assert!(!pace.due(TickTrigger::Timer, PERIOD, at(base, 40)));
        pace.ticked(TickTrigger::HostFrame, PERIOD, at(base, 45), false);
        assert_eq!(pace.next_due(PERIOD, at(base, 46)), at(base, 80));
    }

    /// The compositor's loop: the timer is armed only when it fires, a little late as a real
    /// timer does, a client commits `draws` after each frame's callback and then waits for the
    /// next, and a commit in an open window is served as it lands. The frames a run renders in
    /// two seconds at 50 fps.
    fn frames_of_a_callback_client(draws: Duration) -> u32 {
        let base = Instant::now();
        let end = base + Duration::from_secs(2);
        let mut pace = FramePace::windowed();
        pace.ticked(TickTrigger::Timer, PERIOD, base, false);
        let mut frames = 1;
        let late = Duration::from_micros(100);
        let mut fires = pace.next_wake(PERIOD, base, false) + late;
        let mut commit = Some(base + draws);
        let mut pending = false;
        loop {
            let now = commit.map_or(fires, |c| c.min(fires));
            if now >= end {
                return frames;
            }
            let fired = now == fires;
            if commit == Some(now) {
                commit = None;
                pending = true;
            }
            let trigger = if pending && pace.due(TickTrigger::Damage, PERIOD, now) {
                Some(TickTrigger::Damage)
            } else if fired && pace.due(TickTrigger::Timer, PERIOD, now) {
                Some(TickTrigger::Timer)
            } else {
                None
            };
            if let Some(trigger) = trigger {
                pace.ticked(trigger, PERIOD, now, false);
                frames += 1;
                pending = false;
                commit = Some(now + draws);
            }
            if fired {
                fires = pace.next_wake(PERIOD, now, pending).max(now) + late;
            }
        }
    }

    /// A client that waits for its callback goes quiet after its commit, so only the timer can
    /// take a commit that landed in a window a frame already took: it wakes as each window opens,
    /// and the client keeps the cadence however early in the period it commits, and the extra
    /// frames at most. A commit taken ahead of its window may take the one that ends just past
    /// the run, one frame more.
    #[test]
    fn a_client_drawing_on_frame_callbacks_keeps_the_cadence() {
        let most = 102.0 + EXTRA_CAP + 100.0 * EXTRA_REFILL;
        for draws_us in [100, 2_000, 10_000, 19_000] {
            let frames = frames_of_a_callback_client(Duration::from_micros(draws_us));
            assert!(
                frames >= 99 && f64::from(frames) <= most,
                "{frames} frames in two seconds at 50 fps, drawing {draws_us} us"
            );
        }
    }

    /// The window a moment falls in opened on the boundary before it, whichever frame closed the
    /// last window: a timer's at the window's end or a change's as it landed.
    #[test]
    fn a_window_opens_on_its_boundary() {
        let base = Instant::now();
        let mut pace = FramePace::windowed();
        assert_eq!(pace.window_opened(PERIOD, base), None);
        pace.ticked(TickTrigger::Timer, PERIOD, base, false);
        assert_eq!(pace.window_opened(PERIOD, at(base, 1)), Some(base));
        pace.ticked(TickTrigger::Damage, PERIOD, at(base, 3), false);
        assert_eq!(pace.window_opened(PERIOD, at(base, 4)), Some(base));
        assert_eq!(pace.window_opened(PERIOD, at(base, 20)), Some(at(base, 20)));
        assert_eq!(pace.window_opened(PERIOD, at(base, 45)), Some(at(base, 40)));
    }

    /// With nothing waiting the timer sleeps past a window already open to its end, so a still
    /// screen does not spin it, and a change a frame could not take keeps it close.
    #[test]
    fn the_timer_wakes_for_an_opening_window_and_not_an_open_one() {
        let base = Instant::now();
        let mut pace = FramePace::windowed();
        pace.ticked(TickTrigger::Timer, PERIOD, base, false);
        assert_eq!(pace.next_wake(PERIOD, at(base, 1), false), at(base, 20));
        assert!(pace.next_wake(PERIOD, at(base, 1), true) <= at(base, 1));
        pace.ticked(TickTrigger::Damage, PERIOD, at(base, 3), false);
        assert_eq!(pace.next_wake(PERIOD, at(base, 4), false), at(base, 13));
        pace.defer(at(base, 70));
        assert_eq!(pace.next_wake(PERIOD, at(base, 4), false), at(base, 13));
        assert_eq!(pace.next_wake(PERIOD, at(base, 21), false), at(base, 70));
    }

    /// A timer that fires late, as a busy capture or compositor thread makes it, still ends the
    /// window it was due for: streaming, the frames sent in 1000 periods stay 1000 whether the
    /// ticks come on time or up to 0.9 of a period late (moving the grid to each late tick lost
    /// 9% of them at 0.6 and 23% at 0.9).
    #[test]
    fn late_timer_ticks_keep_the_rate() {
        let base = Instant::now();
        for max_late in [0.0, 0.3, 0.6, 0.9] {
            let mut pace = FramePace::windowed();
            pace.record_tick(TickTrigger::Timer, PERIOD, base, true);
            let (mut seed, mut frames) = (12345u64, 1);
            loop {
                seed = seed
                    .wrapping_mul(6364136223846793005)
                    .wrapping_add(1442695040888963407);
                let late = (seed >> 33) as f64 / (1u64 << 31) as f64 * max_late;
                let at = pace.next_due(PERIOD, base) + PERIOD.mul_f64(late);
                if at > base + PERIOD * 1000 {
                    break;
                }
                pace.record_tick(TickTrigger::Timer, PERIOD, at, true);
                frames += 1;
            }
            assert!(
                (1000..=1001).contains(&frames),
                "{frames} frames at up to {max_late} late"
            );
        }
    }

    /// A screen changing a little slower than the cadence, captured by a loop whose encode now
    /// and then runs past a period: a change taken after the open window's timer was due ends
    /// that window, as a late timer tick does, so 1000 periods still send 1000 frames (closing
    /// the window it landed in gave up the overdue one, 924 frames here).
    #[test]
    fn a_change_taken_late_keeps_the_rate() {
        let base = Instant::now();
        let end = base + PERIOD * 1000;
        let mut pace = FramePace::windowed();
        pace.record_tick(TickTrigger::Timer, PERIOD, base, true);
        let (mut t, mut frames, mut taken, mut k) = (base, 1, base, 0u32);
        let mut change = base;
        while t < end {
            // The loop's wait: a pending change once the window opens, else the timer.
            let due = pace.next_due(PERIOD, t);
            let start = t.max(pace.pull_at(TickTrigger::Damage, PERIOD, t).min(due));
            while change <= taken {
                change += PERIOD.mul_f64(1.3);
            }
            let (now, trigger) = if change <= due.max(start) {
                (start.max(change), TickTrigger::Damage)
            } else {
                (due.max(t), TickTrigger::Timer)
            };
            if now > end {
                break;
            }
            taken = now;
            pace.record_tick(trigger, PERIOD, now, true);
            frames += 1;
            k += 1;
            t = now + PERIOD.mul_f64(if k % 6 == 0 { 1.25 } else { 0.77 });
        }
        assert!(
            (999..=1001).contains(&frames),
            "{frames} frames in 1000 periods"
        );
    }

    /// The earliest pull and the pull gate agree, so a caller may wait for the one and skip
    /// the other: a fresh capture pulls at once, a full budget buys half a period, and a spent
    /// one leaves the timer its whole period.
    #[test]
    fn the_earliest_pull_is_the_instant_the_gate_opens() {
        let base = Instant::now();
        let mut pace = FramePace::default();
        assert_eq!(
            pace.pull_at(TickTrigger::Damage, PERIOD, base),
            base,
            "a fresh capture waits for nothing"
        );
        pace.ticked(TickTrigger::Timer, PERIOD, base, false);
        let earliest = pace.pull_at(TickTrigger::Damage, PERIOD, base);
        assert_eq!(earliest, at(base, 10), "a whole budget buys half a period");
        assert!(pace.due(TickTrigger::Damage, PERIOD, earliest));
        assert!(!pace.due(
            TickTrigger::Damage,
            PERIOD,
            earliest - Duration::from_millis(1)
        ));
        pace.ticked(TickTrigger::Damage, PERIOD, earliest, false);
        assert_eq!(
            pace.pull_at(TickTrigger::Damage, PERIOD, earliest),
            at(base, 30),
            "a spent budget waits the period out"
        );
    }

    #[test]
    fn a_fresh_capture_is_due_at_once() {
        let pace = FramePace::default();
        let now = Instant::now();
        assert!(pace.due(TickTrigger::Timer, PERIOD, now));
        assert!(pace.due(TickTrigger::Input, PERIOD, now));
        assert_eq!(pace.next_due(PERIOD, now), now);
    }

    #[test]
    fn the_timer_ticks_at_its_period_with_jitter_slack() {
        let base = Instant::now();
        let mut pace = FramePace::default();
        pace.ticked(TickTrigger::Timer, PERIOD, base, false);
        assert!(!pace.due(TickTrigger::Timer, PERIOD, at(base, 17)));
        assert!(pace.due(TickTrigger::Timer, PERIOD, at(base, 18)));
        assert_eq!(pace.next_due(PERIOD, at(base, 5)), at(base, 20));
    }

    #[test]
    fn input_pulls_a_frame_forward_from_a_budget() {
        let base = Instant::now();
        let mut pace = FramePace::default();
        pace.ticked(TickTrigger::Timer, PERIOD, base, false);
        assert!(
            !pace.due(TickTrigger::Input, PERIOD, at(base, 9)),
            "under half a period waits for the timer"
        );
        assert!(pace.due(TickTrigger::Input, PERIOD, at(base, 10)));
        pace.ticked(TickTrigger::Input, PERIOD, at(base, 10), false);
        assert_eq!(
            pace.next_due(PERIOD, at(base, 10)),
            at(base, 30),
            "a pull off the cadence leaves the timer its period"
        );
        assert!(pace.due(TickTrigger::Timer, PERIOD, at(base, 30)));
        assert!(
            !pace.due(TickTrigger::Input, PERIOD, at(base, 25)),
            "the budget is spent for a second pull"
        );
        assert!(
            pace.due(TickTrigger::Input, PERIOD, at(base, 30)),
            "a whole period on, input takes the due frame"
        );
        pace.ticked(TickTrigger::Input, PERIOD, at(base, 30), false);
        // About 4 ms refilled by now: a pull of 10 ms does not fit, one of 3 ms does.
        pace.ticked(TickTrigger::Timer, PERIOD, at(base, 120), false);
        assert!(
            !pace.due(TickTrigger::Input, PERIOD, at(base, 130)),
            "a whole pull needs the whole budget back"
        );
        assert!(
            pace.due(TickTrigger::Input, PERIOD, at(base, 137)),
            "a small pull fits what refilled"
        );
        pace.ticked(TickTrigger::Input, PERIOD, at(base, 137), false);
        pace.ticked(TickTrigger::Timer, PERIOD, at(base, 500), false);
        assert!(
            pace.due(TickTrigger::Input, PERIOD, at(base, 510)),
            "the budget is whole again"
        );
    }

    #[test]
    fn input_a_hair_slower_than_the_cadence_renders_every_move() {
        let base = Instant::now();
        let mut pace = FramePace::default();
        pace.ticked(TickTrigger::Timer, PERIOD, base, false);
        // A client pacing at 20.4 ms against a 20 ms period: its first move lands late in the
        // period and pulls its frame; each later move lands after a whole period and renders
        // at once, and no timer tick falls between two of them.
        let spacing = Duration::from_micros(20_400);
        assert!(FramePace::input_paced(Some(spacing), PERIOD));
        let mut t = 16.0;
        pace.ticked(TickTrigger::Input, PERIOD, at(base, 16), true);
        for _ in 0..50 {
            t += 20.4;
            let now = base + Duration::from_micros((t * 1000.0) as u64);
            assert!(
                pace.next_due(PERIOD, now) > now,
                "no timer frame slipped in ahead of the move"
            );
            assert!(
                pace.due(TickTrigger::Input, PERIOD, now),
                "the move renders its own frame"
            );
            pace.ticked(TickTrigger::Input, PERIOD, now, true);
        }
        let stopped = base + Duration::from_micros((t * 1000.0) as u64);
        assert_eq!(
            pace.next_due(PERIOD, stopped),
            stopped + Duration::from_millis(25),
            "once the moves stop, the timer resumes after the grace"
        );
    }

    #[test]
    fn a_pull_that_arrives_late_pays_the_budget_back() {
        let base = Instant::now();
        let mut pace = FramePace::default();
        pace.ticked(TickTrigger::Timer, PERIOD, base, false);
        pace.ticked(TickTrigger::Input, PERIOD, at(base, 10), true);
        assert!(
            !pace.due(TickTrigger::Input, PERIOD, at(base, 25)),
            "the budget is spent"
        );
        pace.ticked(TickTrigger::Input, PERIOD, at(base, 40), true);
        assert!(
            pace.due(TickTrigger::Input, PERIOD, at(base, 50)),
            "a pull 10 ms late earned a 10 ms pull back"
        );
    }

    #[test]
    fn only_input_at_the_cadence_holds_the_timer_back() {
        let base = Instant::now();
        let mut pace = FramePace::default();
        pace.ticked(TickTrigger::Timer, PERIOD, base, false);
        assert!(
            !FramePace::input_paced(Some(Duration::from_millis(7)), PERIOD),
            "a 143 Hz client is not paced at 50 Hz"
        );
        assert!(
            !FramePace::input_paced(Some(Duration::from_millis(40)), PERIOD),
            "nor is one at half the rate"
        );
        assert!(!FramePace::input_paced(None, PERIOD), "nor no input at all");
        pace.ticked(TickTrigger::Input, PERIOD, at(base, 12), false);
        assert_eq!(
            pace.next_due(PERIOD, at(base, 12)),
            at(base, 32),
            "an unpaced pull leaves the timer its period"
        );
        pace.ticked(TickTrigger::Input, PERIOD, at(base, 32), true);
        assert_eq!(
            pace.next_due(PERIOD, at(base, 32)),
            at(base, 57),
            "a paced one adds the grace"
        );
        pace.ticked(TickTrigger::HostFrame, PERIOD, at(base, 57), true);
        assert_eq!(
            pace.next_due(PERIOD, at(base, 57)),
            at(base, 77),
            "a host frame never does"
        );
    }

    #[test]
    fn damage_at_the_cadence_renders_every_frame_through_vblank_jitter() {
        let base = Instant::now();
        let mut pace = FramePace::default();
        // An application presenting on a 20 ms vblank that fires on a millisecond grid, so its
        // frames land 19, 20, or 21 ms apart: each is published as it lands, and no timer
        // tick falls between two of them once the spacing reads as paced.
        let steps = [21, 19, 20, 21, 21, 19, 20, 19, 21, 20];
        let mut t = 0u64;
        pace.ticked(TickTrigger::Damage, PERIOD, at(base, t), false);
        for (i, step) in steps.iter().cycle().take(60).enumerate() {
            t += step;
            let now = at(base, t);
            if i >= 2 {
                assert!(
                    pace.next_due(PERIOD, now) > now,
                    "no timer frame slipped in ahead of frame {i}"
                );
            }
            assert!(
                pace.due(TickTrigger::Damage, PERIOD, now),
                "frame {i} renders as it lands"
            );
            pace.ticked(TickTrigger::Damage, PERIOD, now, false);
        }
        assert_eq!(
            pace.next_due(PERIOD, at(base, t)),
            at(base, t + 25),
            "once it stops, the timer resumes after the grace"
        );
    }

    #[test]
    fn damage_off_the_cadence_leaves_the_timer_its_period() {
        let base = Instant::now();
        let mut pace = FramePace::default();
        // A pointer sprite moving at 125 Hz against a 20 ms period.
        for i in 0..40 {
            pace.ticked(TickTrigger::Damage, PERIOD, at(base, 8 * i), false);
        }
        assert_eq!(pace.next_due(PERIOD, at(base, 312)), at(base, 332));
    }

    #[test]
    fn a_host_frame_pulls_forward_like_input() {
        let base = Instant::now();
        let mut pace = FramePace::default();
        pace.ticked(TickTrigger::Timer, PERIOD, base, false);
        assert!(
            !pace.due(TickTrigger::HostFrame, PERIOD, at(base, 9)),
            "under half a period waits for the timer"
        );
        assert!(pace.due(TickTrigger::HostFrame, PERIOD, at(base, 10)));
        pace.ticked(TickTrigger::HostFrame, PERIOD, at(base, 10), false);
        assert_eq!(
            pace.next_due(PERIOD, at(base, 10)),
            at(base, 30),
            "the cadence follows the host's frames"
        );
        assert!(
            !pace.due(TickTrigger::Timer, PERIOD, at(base, 20)),
            "the old phase's tick is skipped"
        );
        assert!(
            !pace.due(TickTrigger::HostFrame, PERIOD, at(base, 25)),
            "the budget is spent for a second pull"
        );
        assert!(
            pace.due(TickTrigger::HostFrame, PERIOD, at(base, 30)),
            "a whole period on, the frame is due"
        );
    }

    #[test]
    fn a_deferred_tick_holds_the_timer_without_counting_a_frame() {
        let base = Instant::now();
        let mut pace = FramePace::default();
        pace.ticked(TickTrigger::Timer, PERIOD, base, false);
        pace.defer(at(base, 30));
        assert_eq!(
            pace.next_due(PERIOD, at(base, 20)),
            at(base, 30),
            "the timer waits where it was held to"
        );
        assert!(
            pace.due(TickTrigger::HostFrame, PERIOD, at(base, 21)),
            "a frame a period on is still due at once"
        );
        pace.ticked(TickTrigger::HostFrame, PERIOD, at(base, 21), false);
        assert_eq!(
            pace.next_due(PERIOD, at(base, 21)),
            at(base, 41),
            "a rendered frame lifts the hold"
        );
    }

    #[test]
    fn a_timer_that_wakes_late_keeps_its_rate() {
        let base = Instant::now();
        let mut pace = FramePace::default();
        let end = base + Duration::from_secs(2);
        let (mut frames, mut t) = (0, base);
        loop {
            t = pace.next_due(PERIOD, t) + Duration::from_micros(700);
            if t >= end {
                break;
            }
            assert!(pace.due(TickTrigger::Timer, PERIOD, t));
            pace.ticked(TickTrigger::Timer, PERIOD, t, false);
            frames += 1;
        }
        assert!(
            (99..=100).contains(&frames),
            "{frames} frames in two seconds at 50 fps"
        );
    }

    #[test]
    fn a_stalled_timer_starts_its_cadence_again_rather_than_catch_up() {
        let base = Instant::now();
        let mut pace = FramePace::default();
        pace.ticked(TickTrigger::Timer, PERIOD, base, false);
        pace.ticked(TickTrigger::Timer, PERIOD, at(base, 75), false);
        assert_eq!(
            pace.next_due(PERIOD, at(base, 75)),
            at(base, 95),
            "a tick a period and more late counts from when it ran"
        );
    }

    /// The X11 capture's wait: the earliest a damage pull may render, else the timer, with a
    /// change always waiting to be read, as a screen redrawn faster than the cadence has.
    #[test]
    fn damage_faster_than_the_period_holds_the_rate() {
        let base = Instant::now();
        let mut pace = FramePace::default();
        let end = base + Duration::from_secs(2);
        let (mut frames, mut t) = (0, base);
        loop {
            let due = pace.next_due(PERIOD, t);
            t = pace.pull_at(TickTrigger::Damage, PERIOD, t).min(due).max(t);
            if t >= end {
                break;
            }
            pace.ticked(TickTrigger::Damage, PERIOD, t, false);
            frames += 1;
            t += Duration::from_micros(100);
        }
        assert!(
            (100..=101).contains(&frames),
            "{frames} frames in two seconds at 50 fps"
        );
    }

    /// The X11 captures' wait on a screen that stands still between changes: their still ticks
    /// give each change the whole pull, so a change is due as it lands from half a period after
    /// a tick on, however many came before it (counted without it, the changes spent the budget
    /// and waited for the timer), and where the still ticks were sent the frame after it waits a
    /// whole period. It holds whether a still tick sends a frame (streaming mode, and XShm
    /// always) or publishes nothing.
    #[test]
    fn changes_between_still_periods_pull_as_a_fresh_capture_does() {
        for sent in [true, false] {
            changes_between_still_periods_pull(sent);
        }
    }

    fn changes_between_still_periods_pull(sent: bool) {
        let base = Instant::now();
        let mut pace = FramePace::default();
        // A capture's first frame is always sent.
        pace.record_tick(TickTrigger::Timer, PERIOD, base, true);
        let mut t = base;
        for i in 0..40u32 {
            let change = t + PERIOD * 16 + Duration::from_micros(500 + u64::from(i) * 470 % 19_000);
            let mut tick = t;
            loop {
                let due = pace.next_due(PERIOD, t);
                if due >= change {
                    break;
                }
                t = due;
                tick = t;
                pace.record_tick(TickTrigger::Timer, PERIOD, t, sent);
            }
            let at = pace
                .pull_at(TickTrigger::Damage, PERIOD, change)
                .max(change);
            assert!(
                at <= change.max(tick + PERIOD / 2),
                "change {i} (still ticks sent: {sent}) waited {:?}",
                at - change
            );
            pace.record_tick(TickTrigger::Damage, PERIOD, at, true);
            // Where the still ticks were sent, the pull spent a fresh capture's budget whole; where
            // none was, the change came a still spell after the last frame, which refilled it. The
            // first change follows the first frame, which is sent on a screen still till then.
            let left = if sent || i == 0 {
                Duration::ZERO
            } else {
                PERIOD / 2
            };
            assert_eq!(
                pace.pull_at(TickTrigger::Damage, PERIOD, at),
                at + PERIOD - left,
                "the change after change {i} (still ticks sent: {sent}) borrows {left:?}"
            );
            t = at;
        }
    }

    /// The screen stands still only once a period and a half has passed without a change: the
    /// timer's tick after a change's frame, graced or not, still counts as a frame.
    #[test]
    fn the_screen_is_still_a_period_and_a_half_after_its_last_change() {
        let base = Instant::now();
        let mut pace = FramePace::default();
        assert!(pace.still(PERIOD, base), "a capture with no change yet");
        pace.ticked(TickTrigger::Damage, PERIOD, base, false);
        assert!(
            !pace.still(PERIOD, at(base, 25)),
            "the graced timer's tick after a change's frame"
        );
        assert!(!pace.still(PERIOD, at(base, 29)));
        assert!(pace.still(PERIOD, at(base, 30)), "a period and a half on");
    }

    #[test]
    fn input_faster_than_the_period_raises_the_rate_by_the_refill_at_most() {
        let base = Instant::now();
        let mut pace = FramePace::default();
        let end = base + Duration::from_secs(2);
        let (mut frames, mut next_input, mut next_timer) = (0, base, base);
        loop {
            let t = next_input.min(next_timer);
            if t >= end {
                break;
            }
            if t == next_timer {
                if pace.due(TickTrigger::Timer, PERIOD, t) {
                    pace.ticked(TickTrigger::Timer, PERIOD, t, false);
                    frames += 1;
                }
                next_timer = pace.next_due(PERIOD, t);
            }
            if t == next_input {
                if pace.due(TickTrigger::Input, PERIOD, t) {
                    pace.ticked(TickTrigger::Input, PERIOD, t, false);
                    frames += 1;
                    next_timer = pace.next_due(PERIOD, t);
                }
                next_input = t + Duration::from_millis(7);
            }
        }
        assert!(
            (100..=104).contains(&frames),
            "{frames} frames in two seconds at 50 fps"
        );
    }
}
