/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! The force feedback a pad's applications play, mixed into one rumble for the client.
//!
//! Applications play rumble on a virtual pad the way they do on an Xbox pad the kernel
//! drives through its memless force-feedback layer: effects are uploaded, then played for
//! their length times a repeat count, or until stopped when the length is 0, after an
//! optional delay, and the motors take the sum of everything playing, scaled by the gain
//! and capped. Each source of effects (an interposer evdev connection, the kernel device)
//! reports them as [`FfRecord`]s, the 16-byte `sji_ff_record_t` the interposer writes back
//! on its socket and `UInputGamepad::service_ff` produces from the kernel's requests. The
//! mixer keeps what plays, in loop time, and hands the client a level for each motor and
//! how long to hold it: until the next effect starts or ends, or, where that is further off
//! than [`FF_LEASE_MS`] or never (an effect without an end), for a lease at a time, renewed
//! at half that; the Gamepad API plays at most 5 s at once, and a client that stops hearing
//! stops shaking. A mix that did not change is not sent again until a renewal is due. The
//! mixer is pure over the instants it is given, so the pad drives it from its own timer.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use crate::gamepad::abi::{read_i16, read_u16};

/// A force-feedback record's kind: a play (`count` repeats, 0 stopping), a stop (the effect
/// erased), or the gain (`count` carries it, 0 to 0xffff).
pub const FF_RECORD_PLAY: u8 = 1;
pub const FF_RECORD_STOP: u8 = 2;
pub const FF_RECORD_GAIN: u8 = 3;
/// The 16 bytes of `sji_ff_record_t`: kind, reserved, effect id (`i16`), strong, weak,
/// length ms, delay ms, repeat count or gain, reserved.
pub const FF_RECORD_SIZE: usize = 16;
/// How long a rumble without its own end is played for at a time; it is renewed at half
/// this while it plays, so a client that stops hearing stops shaking.
pub const FF_LEASE_MS: u32 = 2000;

/// One force-feedback request an application made on a pad handle.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FfRecord {
    pub kind: u8,
    pub effect_id: i16,
    pub strong: u16,
    pub weak: u16,
    pub length_ms: u16,
    pub delay_ms: u16,
    /// The repeat count of a play, or the gain of a gain record.
    pub count: u16,
}

impl FfRecord {
    pub fn play(
        effect_id: i16,
        strong: u16,
        weak: u16,
        length_ms: u16,
        delay_ms: u16,
        count: u16,
    ) -> FfRecord {
        FfRecord {
            kind: FF_RECORD_PLAY,
            effect_id,
            strong,
            weak,
            length_ms,
            delay_ms,
            count,
        }
    }

    pub fn stop(effect_id: i16) -> FfRecord {
        FfRecord {
            kind: FF_RECORD_STOP,
            effect_id,
            strong: 0,
            weak: 0,
            length_ms: 0,
            delay_ms: 0,
            count: 0,
        }
    }

    pub fn gain(gain: u16) -> FfRecord {
        FfRecord {
            kind: FF_RECORD_GAIN,
            effect_id: 0,
            strong: 0,
            weak: 0,
            length_ms: 0,
            delay_ms: 0,
            count: gain,
        }
    }

    /// Parse the 16 bytes an interposer handle wrote.
    pub fn parse(buf: &[u8]) -> Option<FfRecord> {
        if buf.len() < FF_RECORD_SIZE {
            return None;
        }
        Some(FfRecord {
            kind: buf[0],
            effect_id: read_i16(buf, 2),
            strong: read_u16(buf, 4),
            weak: read_u16(buf, 6),
            length_ms: read_u16(buf, 8),
            delay_ms: read_u16(buf, 10),
            count: read_u16(buf, 12),
        })
    }
}

/// Where an effect was played from: an interposer connection, or the kernel device.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum FfSource {
    Conn(u64),
    Uinput,
}

/// What the client is told to play: each motor's level, 0 to 1, and how long to hold it in
/// ms; both levels 0 with 0 ms stops it.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Rumble {
    pub strong: f64,
    pub weak: f64,
    pub hold_ms: u32,
}

/// An effect playing or waiting out its delay.
struct Playing {
    strong: u32,
    weak: u32,
    start: Instant,
    end: Option<Instant>,
}

/// The per-pad mix of playing effects.
pub struct FfMixer {
    playing: HashMap<(FfSource, i16), Playing>,
    gain: u32,
    /// The mix last handed on; `None` forces the next update to hand the mix on again.
    sent: Option<(u16, u16)>,
    renew_at: Option<Instant>,
    wake: Option<Instant>,
}

impl Default for FfMixer {
    fn default() -> Self {
        FfMixer {
            playing: HashMap::new(),
            gain: 0xffff,
            sent: Some((0, 0)),
            renew_at: None,
            wake: None,
        }
    }
}

impl FfMixer {
    /// Apply one record a source produced, then remix.
    ///
    /// A play starts after its delay and lasts its length times its repeat count, or until
    /// stopped when its length is 0; a play of an effect already playing replaces it, as a
    /// replay does on a kernel pad.
    pub fn record(&mut self, source: FfSource, record: FfRecord, now: Instant) -> Option<Rumble> {
        let key = (source, record.effect_id);
        match record.kind {
            FF_RECORD_PLAY if record.count > 0 => {
                let start = now + Duration::from_millis(record.delay_ms as u64);
                let end = (record.length_ms > 0).then(|| {
                    start + Duration::from_millis(record.count as u64 * record.length_ms as u64)
                });
                self.playing.insert(
                    key,
                    Playing {
                        strong: record.strong as u32,
                        weak: record.weak as u32,
                        start,
                        end,
                    },
                );
            }
            FF_RECORD_PLAY | FF_RECORD_STOP => {
                self.playing.remove(&key);
            }
            FF_RECORD_GAIN => self.gain = record.count as u32,
            _ => {}
        }
        self.update(now)
    }

    /// Hand the mix playing now on again, for a client that has just taken the slot: an
    /// unchanged mix is otherwise not sent until its renewal. A quiet pad hands on nothing.
    pub fn replay(&mut self, now: Instant) -> Option<Rumble> {
        match self.sent {
            Some((0, 0)) => None,
            _ => {
                self.sent = None;
                self.update(now)
            }
        }
    }

    /// Stop every effect one source played: its handle closed, or its device went.
    pub fn forget(&mut self, source: FfSource, now: Instant) -> Option<Rumble> {
        self.playing.retain(|(s, _), _| *s != source);
        self.update(now)
    }

    /// When the mix next has to be recomputed: the next start, end, or lease renewal.
    pub fn next_wake(&self) -> Option<Instant> {
        self.wake
    }

    /// Mix the effects playing at `now` and say what to hand the client, if anything.
    pub fn update(&mut self, now: Instant) -> Option<Rumble> {
        self.playing
            .retain(|_, p| p.end.is_none_or(|end| end > now));
        let (mut strong, mut weak) = (0u32, 0u32);
        let mut boundaries: Vec<Instant> = Vec::new();
        let mut endless = false;
        for p in self.playing.values() {
            if p.start > now {
                boundaries.push(p.start);
                continue;
            }
            strong += p.strong;
            weak += p.weak;
            match p.end {
                None => endless = true,
                Some(end) => boundaries.push(end),
            }
        }
        let strong = (strong as u64 * self.gain as u64 / 0xffff).min(0xffff) as u16;
        let weak = (weak as u64 * self.gain as u64 / 0xffff).min(0xffff) as u16;
        let mix = (strong, weak);
        let audible = strong > 0 || weak > 0;
        let lease = Duration::from_millis(FF_LEASE_MS as u64);
        let change = boundaries.iter().copied().filter(|t| *t > now).min();
        let leased = audible && (endless || change.is_none_or(|c| c - now > lease));
        let hold = if leased {
            lease
        } else {
            match change {
                Some(c) if audible => c - now,
                _ => Duration::ZERO,
            }
        };
        let renew = self.renew_at.is_some_and(|at| now >= at);
        let mut out = None;
        if self.sent != Some(mix) || renew {
            self.sent = Some(mix);
            self.renew_at = leased.then(|| now + lease / 2);
            out = Some(Rumble {
                strong: strong as f64 / 65535.0,
                weak: weak as f64 / 65535.0,
                hold_ms: (hold.as_secs_f64() * 1000.0).round() as u32,
            });
        }
        let mut wake: Vec<Instant> = boundaries.into_iter().filter(|t| *t > now).collect();
        if let Some(at) = self.renew_at {
            wake.push(at);
        }
        self.wake = wake.into_iter().min();
        out
    }
}
