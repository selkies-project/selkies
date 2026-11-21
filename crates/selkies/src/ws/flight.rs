/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! What a socket's path still holds of what its writer handed it, read in band.
//!
//! The writer puts a ping behind the bulk frames it writes (`Flight::wrote`), video and
//! padding, to a page that stamps its acks (below; no other is pinged), and the page's
//! engine answers each at once, so a pong says everything written
//! before its ping has arrived: what was written after the newest ping answered is in flight,
//! on its way through this host's send buffer, the path's queues and whatever proxy stands
//! between. The kernel's own count (`TCP_INFO`) ends at the first proxy that terminates the
//! connection, and a send buffer and a bottleneck's queue together hold seconds of a stream
//! once the path narrows, which every later byte, audio among them, then waits behind.
//!
//! A ping that stands out says nothing by itself of where it stands: a path that narrowed
//! keeps it on its way out, and an upload filling the page's uplink keeps its pong on the way
//! back, for as long, with the stream arriving on time. The page tells them apart: it stamps
//! each ack it sends with its own clock as the ack leaves (`note_stamp`), on every tick of
//! its ack timer while frames reach it. The socket keeps the order of what the page sends, so
//! a stamp that arrives says every pong the page had sent by then has arrived too, and a ping
//! still out had not reached the page when the stamp left. How far behind this clock a stamp
//! arrives, past the least any did, is how long its way back held it; taken off its arrival,
//! that is when it left, on this clock. For `FLIGHT_STAMP_S` after a stamp arrived, the next
//! not being due, the way back is taken to hold what it held that one.
//!
//! A path is late once its oldest unanswered ping has stood on its way out, by the page's
//! last stamp, `FLIGHT_LATE_S` past the path's least round trip, and past that by
//! `FLIGHT_JITTER_SLACK` times how far its round trips lay apart while nothing was paced: a
//! path that delivers unevenly is not late for it. From then on bulk frames are paced to what
//! the path delivers: one goes once no bulk frame is in flight any more, or while what is in
//! flight would cross within the least round trip and `FLIGHT_LATE_S` (or as long again as
//! the round trip, on a path longer than that), and what the way back holds, at the rate the
//! pongs show. Each pong gives a rate: what arrived between it and a pong at least
//! `FLIGHT_RATE_SPAN_S` before it, over the time between them, which is the path's rate while
//! it had more to deliver and less while it stood idle; so the largest of the last
//! `FLIGHT_RATE_WINDOW_S` counts, and the rates read before the path went late are let go. A
//! frame the pacing holds waits in the writer, and the frames behind it with their relay,
//! where the display's gate still counts them and can let them go, instead of in the path
//! ahead of the audio written after them.
//!
//! The pacing is for a path that narrowed under the stream, and ends where that is not, or
//! no longer, so:
//! - with a bulk frame that finds none in flight, or the path on time and what is in flight
//!   fitting, and has no other waiting behind it (the stream is taking less than the path
//!   carries);
//! - once two pongs of what was written before the path showed late come back as far apart
//!   as their pings went out, within `FLIGHT_KEEPS_UP`: the path carried those bytes at the
//!   pace they were written, neither slower (it is narrowed) nor faster (it is draining a
//!   queue the stream put there), so the pings stood out for a longer way round or a queue
//!   other traffic keeps, and the least round trip seen since, less what the way back held,
//!   is the path's from then on. Only pings written while nothing was paced, and not as the
//!   socket made room, can say: what is written while the path is paced, or while the
//!   socket pushes back, goes out as it is delivered;
//! - with a bulk frame that asks after none did for `FLIGHT_RATE_WINDOW_S` and finds none in
//!   flight (the stream had stopped);
//! - after `FLIGHT_PACED_MAX_S` on end, the least round trip of the stretch becoming the
//!   floor;
//! - once the page's last stamp is `FLIGHT_DISTRUST_S` old.
//!
//! Where the pongs could not show the pace kept (too few pings out, pongs bunched behind a
//! stall), a longer way round still shows in the pings that went out behind a bulk frame that
//! found none in flight: their round trips hold the path's and that one frame's crossing, and
//! no queue of the stream's. Once `FLIGHT_SETTLE_PONGS` of those stood past the late line
//! through `FLIGHT_SETTLE_S`, less what the way back held, no more than `FLIGHT_SETTLE_GAP_S`
//! apart and no pong of any ping coming back within the line meanwhile, the least of them is
//! the floor; the pacing then ends by the rules above. A ping written while a frame is held
//! comes back within the line where the wait was only the frame ahead crossing a narrow path,
//! which keeps that path's floor where it is.
//!
//! A queue that builds slowly, under a path carrying most of the stream, is left to the
//! display's congestion steer, which reads it over its own windows.
//!
//! A peer that has answered no ping is never judged, nor a page that stamps nothing, nor one
//! whose last stamp is `FLIGHT_DISTRUST_S` old, until it stamps again; none of these is
//! pinged either. A page stamps nothing if it is an older one, or a viewer's, which sends no
//! ack; and only the stamps of its display's own page are taken (`Client::note_stamp`'s
//! caller): the gate that counts the frames held and lets them go is that page's alone, and
//! video held for another, a second tab's, would only grow old with its relay.
//!
//! A page that stamps on is there, and answers what reaches it: its pings stand out behind
//! what is still crossing, a key frame over a narrowed path for seconds, or on their way
//! back, and its video stays paced. Only when it has answered none for `FLIGHT_UNANSWERED_S`
//! is it left to the kernel's own backpressure until it answers again. While a frame is held
//! a ping still goes out every `FLIGHT_HELD_PROBE_S`: its pong says what was written has
//! arrived, and answers for any ping before it that the peer let go unanswered.
//!
//! The least a stamp arrived behind this clock is kept over the history the least round trip
//! is (`FloorHistory`), and read only while pongs come, as that one is, so a way back that
//! stays held is told from a path that grew longer for as long as either is remembered, a
//! still screen's pause included. A page whose clock runs slow against this one reads as a
//! way back held that much longer, a tenth of a second over that history at the most for
//! clocks 170 parts in a million apart, and its path is late that much later. One whose clock
//! runs fast is read as it is while its way back is unheld at times, each stamp that arrives
//! sooner becoming the least. Behind a way back held without a break for minutes (a long
//! upload) the least stays a stamp from before; a fast clock gains on it, and the path reads
//! late by that gain: past a tenth of a second, the same 170 parts in a million over ten
//! minutes, frames are paced for moments at a time, each ended by the pace the path kept,
//! until the history forgets that stamp.

use std::collections::VecDeque;

use crate::ws::uplink::FloorHistory;

/// How far past the least round trip a ping stands on its way out before the path is late,
/// and how much of the path's delivery may stand in flight past the round trip while it is.
pub const FLIGHT_LATE_S: f64 = 0.1;
/// The least gap between two pings behind bulk frames that are not paced.
pub const FLIGHT_PROBE_S: f64 = 0.025;
/// The gap between two pings while a bulk frame is held and none is written.
pub const FLIGHT_HELD_PROBE_S: f64 = 0.1;
/// How far back the delivery rates a late path showed count.
pub const FLIGHT_RATE_WINDOW_S: f64 = 0.5;
/// The least stretch of pongs a delivery rate is read over: pongs closer than this arrive
/// bunched as often as the path delivered that fast.
pub const FLIGHT_RATE_SPAN_S: f64 = 0.05;
/// How near the pace the stream was written at, before the path showed late, the path
/// carried it to be neither narrowed under the stream (slower) nor draining a queue of the
/// stream's (faster): this share of it and its inverse.
pub const FLIGHT_KEEPS_UP: f64 = 0.9;
/// A page is not judged by a stamp this old.
pub const FLIGHT_DISTRUST_S: f64 = 2.0;
/// A peer whose page stamps on and that answers no ping for this long is not judged by its
/// pings: as long as the link gives a key frame to cross (`link::SEND_STALL_SECONDS`).
pub const FLIGHT_UNANSWERED_S: f64 = 10.0;
/// How long no round trip comes back within the late line, those with no queue of the
/// stream's in them standing past it, before the least of those is the floor; how many of
/// them at the least, and how far apart at the most.
pub const FLIGHT_SETTLE_S: f64 = 3.0;
pub const FLIGHT_SETTLE_PONGS: usize = 3;
pub const FLIGHT_SETTLE_GAP_S: f64 = 1.5;
/// How long a path is paced on end before the least round trip of the stretch is the floor.
pub const FLIGHT_PACED_MAX_S: f64 = 30.0;
/// The share of each new distance between two round trips taken into the smoothed one.
pub const FLIGHT_JITTER_GAIN: f64 = 0.125;
/// How many times the smoothed distance between round trips a round trip may run past the
/// bound without being late, and the most that may come to.
pub const FLIGHT_JITTER_SLACK: f64 = 4.0;
pub const FLIGHT_SLACK_MAX_S: f64 = 0.4;
/// How often the page stamps an ack while frames reach it (its ack timer): for that long
/// after a stamp arrived, the next not being due, its way back is taken to hold nothing
/// more than it held that one.
pub const FLIGHT_STAMP_S: f64 = 0.05;
/// The stamps the way back's standing delay is read over.
const BACKS: usize = 4;
/// The pings kept outstanding at most.
const PENDING_MAX: usize = 512;
/// The pongs remembered at most, within the rate window.
const ARRIVALS_MAX: usize = 1024;
/// Where these pings' counters start, apart from the uplink's and the transfers'.
const COUNTER_BASE: u64 = 1 << 62;

#[derive(Debug)]
struct Probe {
    counter: u64,
    sent: f64,
    /// Everything written when it went out.
    written: u64,
    /// Whether it went out as the socket made room (behind a write that waited or just after
    /// one) or while a frame was held, when the stream itself would have written it.
    clocked: bool,
    /// Whether it went out behind a bulk frame that found none in flight: its round trip is
    /// the path's and that one frame's crossing, with no queue of the stream's in it.
    alone: bool,
}

/// A pong: when it came back, and its ping.
#[derive(Clone, Copy, Debug)]
struct Arrival {
    at: f64,
    written: u64,
    sent: f64,
    clocked: bool,
}

/// One socket's flight gauge.
#[derive(Debug, Default)]
pub struct Flight {
    pending: VecDeque<Probe>,
    next: u64,
    written: u64,
    delivered: u64,
    /// Everything written when the last bulk frame went out.
    bulk_written: u64,
    probed: f64,
    answered_at: f64,
    /// When a bulk frame last asked to go, and the bound it asked under.
    asked_at: f64,
    bound: f64,
    arrivals: VecDeque<Arrival>,
    /// Delivery rates the pongs showed, in bytes a second, with when.
    rates: VecDeque<(f64, f64)>,
    floors: FloorHistory,
    floor: Option<f64>,
    trusted: bool,
    paced: bool,
    /// How far one round trip lies from the next on this path while nothing is paced,
    /// smoothed, and the last one.
    jitter: f64,
    last_round_trip: f64,
    /// When the path went late, and the least round trip seen since.
    paced_at: f64,
    paced_floor: f64,
    /// When the pacing last ended.
    unpaced_at: f64,
    /// Since when no round trip came back within the late line while those of pings behind
    /// a frame that found none in flight stood past it, the least of those, how many, and
    /// when the last one came.
    over_since: Option<f64>,
    over_floor: f64,
    over_count: usize,
    over_at: f64,
    /// When a write last waited on the socket.
    waited_at: f64,
    /// The page's stamps: how far behind this clock each arrived, the least of it over the
    /// floor's own history; whether one came, when the last did, and how long its way back
    /// held that one past the least.
    lags: FloorHistory,
    stamped: bool,
    stamped_at: f64,
    back: f64,
    /// What the way back held the last few stamps: the least of it stands on that way
    /// whatever one piece is held more than the next.
    backs: VecDeque<f64>,
}

impl Flight {
    /// `len` bytes went to the socket at `now`, a bulk frame or not, after a write that
    /// `waited` on the socket or went at once: the payload of the ping to write behind them
    /// where one is due. A paced path has one behind every bulk frame, so what is in flight
    /// is known to the frame.
    pub fn wrote(&mut self, len: usize, now: f64, bulk: bool, waited: bool) -> Option<Vec<u8>> {
        self.written += len as u64;
        if waited {
            self.waited_at = now;
        }
        if !bulk {
            return None;
        }
        // A path with nothing in flight since a while stood idle: a rate read across that
        // stretch would say nothing of what it carries.
        if self.pending.is_empty() && now - self.answered_at > FLIGHT_RATE_SPAN_S {
            self.arrivals.clear();
        }
        let alone = self.trusted && self.delivered >= self.bulk_written;
        self.bulk_written = self.written;
        // Only a page that stamps is pinged: one that does not is never judged, and gets
        // on its socket what it got before there was a gauge.
        self.mark(now)?;
        let gap = if self.paced { 0.0 } else { FLIGHT_PROBE_S };
        if now - self.probed < gap {
            return None;
        }
        // A write that went at once into the room a wait just made is spaced by the path
        // as the wait was.
        let clocked = self.waited_at > 0.0 && now - self.waited_at < FLIGHT_RATE_SPAN_S;
        self.probe(now, clocked, alone)
    }

    /// The payload of a ping to write at `now` while a bulk frame is held and nothing is
    /// written behind which one would go, where one is due.
    pub fn probe_held(&mut self, now: f64) -> Option<Vec<u8>> {
        if !self.trusted || now - self.probed < FLIGHT_HELD_PROBE_S {
            return None;
        }
        self.probe(now, true, false)
    }

    fn probe(&mut self, now: f64, clocked: bool, alone: bool) -> Option<Vec<u8>> {
        if self.pending.len() >= PENDING_MAX {
            return None;
        }
        self.probed = now;
        let counter = COUNTER_BASE + self.next;
        self.next += 1;
        self.pending.push_back(Probe {
            counter,
            sent: now,
            written: self.written,
            clocked,
            alone,
        });
        Some(counter.to_be_bytes().to_vec())
    }

    /// A pong at `now`: whether it answered one of these pings, which answers every older one
    /// too (the socket keeps them in order).
    pub fn note_pong(&mut self, payload: &[u8], now: f64) -> bool {
        let Ok(bytes) = <[u8; 8]>::try_from(payload) else {
            return false;
        };
        let counter = u64::from_be_bytes(bytes);
        let Some(index) = self.pending.iter().position(|p| p.counter == counter) else {
            return false;
        };
        let Some(probe) = self.pending.drain(..=index).next_back() else {
            return false;
        };
        let round_trip = now - probe.sent;
        let floor = self.floors.observe(round_trip, now);
        self.floor = Some(floor);
        self.trusted = true;
        self.answered_at = now;
        self.delivered = probe.written;
        // A path's own unsteadiness is read while nothing is paced: a queue the stream
        // builds moves the round trips too.
        if !self.paced {
            if self.last_round_trip > 0.0 {
                let apart = (round_trip - self.last_round_trip).abs();
                self.jitter += (apart - self.jitter) * FLIGHT_JITTER_GAIN;
            }
            self.last_round_trip = round_trip;
        }
        // What arrived since a pong at least the span back, over the time since.
        let since = self
            .arrivals
            .iter()
            .rev()
            .find(|arrival| now - arrival.at >= FLIGHT_RATE_SPAN_S)
            .copied();
        if let Some(since) = since
            && probe.written > since.written
        {
            let rate = (probe.written - since.written) as f64 / (now - since.at);
            self.rates.push_back((now, rate));
        }
        while self
            .rates
            .front()
            .is_some_and(|(at, _)| now - at > FLIGHT_RATE_WINDOW_S)
        {
            self.rates.pop_front();
        }
        self.arrivals.push_back(Arrival {
            at: now,
            written: probe.written,
            sent: probe.sent,
            clocked: probe.clocked,
        });
        while self.arrivals.len() > ARRIVALS_MAX
            || (self.arrivals.len() > 1 && now - self.arrivals[1].at > FLIGHT_RATE_WINDOW_S)
        {
            self.arrivals.pop_front();
        }
        // A longer way round shows in every round trip, for as long as it lasts: in those
        // with no queue of the stream's in them, and in none coming back within the line (a
        // ping written while a frame is held comes back early where the wait was the frame
        // ahead crossing a narrow path).
        let out = (round_trip - self.back(now)).max(floor);
        if out <= self.late_line() {
            self.over_since = None;
        } else if probe.alone {
            if self.over_since.is_none() || now - self.over_at > FLIGHT_SETTLE_GAP_S {
                self.over_since = Some(now);
                self.over_floor = out;
                self.over_count = 0;
            }
            self.over_floor = self.over_floor.min(out);
            self.over_count += 1;
            self.over_at = now;
            if self.over_count >= FLIGHT_SETTLE_PONGS
                && self
                    .over_since
                    .is_some_and(|since| now - since >= FLIGHT_SETTLE_S)
            {
                self.move_floor(self.over_floor, now);
            }
        }
        if self.paced {
            self.paced_floor = self.paced_floor.min(out);
            // Two pongs that came back since the path showed late, of pings that went out
            // before it did, while nothing was paced and behind writes the socket took at
            // once: did the bytes between them cross at the pace they were written?
            let kept_up = since.is_some_and(|since| {
                let (apart, sent_apart) = (now - since.at, probe.sent - since.sent);
                since.at >= self.paced_at
                    && since.sent >= self.unpaced_at
                    && probe.sent <= self.paced_at
                    && !since.clocked
                    && !probe.clocked
                    && sent_apart > 0.0
                    && apart <= sent_apart / FLIGHT_KEEPS_UP
                    && apart >= sent_apart * FLIGHT_KEEPS_UP
            });
            // A path paced this long on end has shown its least round trip many times
            // over, whatever the floor says.
            let long = now - self.paced_at > FLIGHT_PACED_MAX_S;
            if (kept_up || long) && self.paced_floor.is_finite() {
                self.move_floor(self.paced_floor, now);
                self.unpace(now);
            }
        }
        true
    }

    /// A stamp that arrived at `now`: `stamp` is the page's own clock, in seconds, as it sent
    /// the message. Everything the page sent before it has arrived (the socket keeps the
    /// order), a pong among them; and how far behind this clock the stamp arrived, past the
    /// least any did, is how long the way back held it.
    pub fn note_stamp(&mut self, stamp: f64, now: f64) {
        if !stamp.is_finite() {
            return;
        }
        let lag = now - stamp;
        // The least is read while pongs come, as the least round trip is: over a still
        // screen, where the page's acks go on and no ping does, neither history moves, and
        // the two forget a way back that has grown longer together.
        let least = if self.lags.is_empty() || now - self.answered_at <= FLIGHT_DISTRUST_S {
            self.lags.observe(lag, now)
        } else {
            self.lags.floor().unwrap_or(lag)
        };
        self.back = (lag - least).max(0.0);
        self.stamped = true;
        self.stamped_at = now;
        self.backs.push_back(self.back);
        while self.backs.len() > BACKS {
            self.backs.pop_front();
        }
    }

    /// The latest moment, on this clock, at which a ping still out is known not to have
    /// reached the page: when the page sent its last stamp, as if its way back held nothing
    /// past the least (no pong still out had been sent by then), and on from there for as
    /// long as its next stamp is not due. `None` where no stamp says, none having come for
    /// `FLIGHT_DISTRUST_S`.
    fn mark(&self, now: f64) -> Option<f64> {
        let since = now - self.stamped_at;
        (self.stamped && since <= FLIGHT_DISTRUST_S)
            .then(|| self.stamped_at - self.back + since.clamp(0.0, FLIGHT_STAMP_S))
    }

    /// How long the way back holds what the page sends, past the least it took: the least
    /// its last few stamps were held, while one says.
    fn back(&self, now: f64) -> f64 {
        if self.mark(now).is_some() {
            self.backs.iter().copied().reduce(f64::min).unwrap_or(0.0)
        } else {
            0.0
        }
    }

    /// The path's least round trip is `floor` from `now` on.
    fn move_floor(&mut self, floor: f64, now: f64) {
        self.floors.reset_to(floor, now);
        self.floor = Some(floor);
        self.over_since = None;
        self.last_round_trip = 0.0;
    }

    /// How long on its way out, and back with nothing held, past which the path is late: the
    /// floor, the bound, and the slack this path's own unsteadiness is given.
    fn late_line(&self) -> f64 {
        self.floor.unwrap_or(0.0) + self.bound + self.slack()
    }

    /// Whether a bulk frame may go at `now` on a path late past `bound`, `more` of them
    /// waiting behind it with its sender, which only a paced path's answer turns on. The
    /// lateness is the page's word (`note_stamp`): with no stamp to say, every frame goes.
    pub fn clear(&mut self, now: f64, bound: f64, more: bool) -> bool {
        let idle = now - self.asked_at > FLIGHT_RATE_WINDOW_S;
        self.asked_at = now;
        self.bound = bound;
        if !self.trusted {
            return true;
        }
        let Some(mark) = self.mark(now) else {
            // No stamp tells this path's way out from its way back: it is not judged.
            if self.paced {
                self.unpace(now);
                self.rates.clear();
            }
            return true;
        };
        if self.delivered >= self.bulk_written {
            // No bulk frame is in flight: the pacing ends with the sender's backlog, or
            // with a stream that had stopped.
            if self.paced && (idle || !more) {
                if idle {
                    // Nothing was paced over the pause: the stretch ended when it began.
                    self.paced = false;
                } else {
                    self.unpace(now);
                }
                self.rates.clear();
            }
            return true;
        }
        let Some(oldest) = self.pending.front().map(|p| p.sent) else {
            // What is in flight has no ping behind it yet to answer for it.
            return true;
        };
        // The page stamps on, so its pings stand out behind what is still crossing or on
        // their way back, however long: up to the span a key frame is given to cross.
        if now - oldest.max(self.answered_at) > FLIGHT_UNANSWERED_S {
            self.pending.clear();
            self.rates.clear();
            self.arrivals.clear();
            self.delivered = self.written;
            self.over_since = None;
            self.trusted = false;
            self.unpace(now);
            return true;
        }
        let floor = self.floor.unwrap_or(0.0);
        // The oldest ping still out had not reached the page when its last stamp left: it
        // has been on its way out for at least that long.
        let late = mark - oldest > self.late_line();
        if late && !self.paced {
            self.paced = true;
            // The rates of the path as it was say nothing of what it carries now.
            self.rates.clear();
            self.paced_at = now;
            self.paced_floor = f64::INFINITY;
            self.last_round_trip = 0.0;
        }
        if !self.paced {
            return true;
        }
        // What is in flight includes a round trip's worth that has arrived and whose pongs
        // are on their way back; past that the bound may stand queued, or on a long path
        // as much again as the round trip, without which the rate read off a window this
        // small would never grow back to what the path carries.
        let in_flight = (self.written - self.delivered) as f64;
        let fits = self
            .rate(now)
            .is_some_and(|rate| in_flight <= rate * (floor + bound.max(floor) + self.back(now)));
        if fits && !late && !more {
            self.unpace(now);
        }
        fits
    }

    /// What a round trip may run past the floor and the bound on this path without being
    /// late: `FLIGHT_JITTER_SLACK` times how far its round trips lay apart while nothing was
    /// paced, a path that delivers unevenly being no later for it.
    fn slack(&self) -> f64 {
        (FLIGHT_JITTER_SLACK * self.jitter).min(FLIGHT_SLACK_MAX_S)
    }

    fn unpace(&mut self, now: f64) {
        if self.paced {
            self.paced = false;
            self.unpaced_at = now;
        }
    }

    /// The most the path delivered a second over the rate window before `now`, in bytes,
    /// where a pong said.
    pub fn rate(&self, now: f64) -> Option<f64> {
        self.rates
            .iter()
            .filter(|(at, _)| now - at <= FLIGHT_RATE_WINDOW_S)
            .map(|(_, rate)| *rate)
            .reduce(f64::max)
    }

    /// Whether bulk frames are paced to what the path delivers.
    pub fn paced(&self) -> bool {
        self.paced
    }

    /// `paced` as of `now`, for who does not ask to send: no longer once no bulk frame is in
    /// flight and none asked to go for `FLIGHT_RATE_WINDOW_S`, or none asked for
    /// `FLIGHT_DISTRUST_S` whatever is in flight, the stream having stopped.
    pub fn paced_at(&self, now: f64) -> bool {
        let asked = now - self.asked_at;
        self.paced
            && asked <= FLIGHT_DISTRUST_S
            && !(self.delivered >= self.bulk_written && asked > FLIGHT_RATE_WINDOW_S)
    }

    /// Whether bulk frames were paced at `now` or within `span` before it: a pacing that
    /// lets up between two frames is the same stretch to who looks every `span`.
    pub fn paced_within(&self, now: f64, span: f64) -> bool {
        self.paced_at(now) || (self.unpaced_at > 0.0 && now - self.unpaced_at <= span)
    }

    /// Bytes written and not yet known to have arrived.
    pub fn in_flight(&self) -> u64 {
        self.written - self.delivered
    }

    /// The path's least round trip in seconds, once a pong has said.
    pub fn floor(&self) -> Option<f64> {
        self.floor
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// How far the page's clock stands from this one.
    const PAGE_CLOCK: f64 = 5_000.0;
    /// The page's ack timer, and how often it repeats an ack with no new frame to tell of.
    const PAGE_TICK: f64 = 0.05;
    const PAGE_HEARTBEAT: f64 = 1.0;

    /// A path of `rate` bytes a second, `down` seconds to the page and `up` back, with a
    /// queue of its own, and the page at its end: its engine answers each ping as it
    /// arrives, and its ack timer, once a first frame has reached it, stamps an ack on each
    /// tick within a second of a frame's arrival and one every second after that. Each way
    /// keeps the order of what it carries.
    struct Path {
        rate: f64,
        down: f64,
        up: f64,
        /// When the path's queue is empty again.
        free_at: f64,
        /// The most a byte waited in the path's queue.
        worst: f64,
        sent: u64,
        /// The most a piece is held back on its way, each way, and where the next draw
        /// comes from.
        jitter: f64,
        draw: u64,
        /// On its way out: (arrival at the page, the ping behind it, whether a bulk frame).
        out: VecDeque<(f64, Option<Vec<u8>>, bool)>,
        last_out: f64,
        /// On its way back: (arrival, what).
        back: VecDeque<(f64, Back)>,
        last_back: f64,
        /// The page's next tick, its last ack, the last frame it got, and whether it stamps.
        tick: f64,
        acked: f64,
        fresh: f64,
        stamping: bool,
        /// How much faster than this clock the page's runs.
        drift: f64,
    }

    enum Back {
        Pong(Vec<u8>),
        Stamp(f64),
    }

    impl Path {
        fn new(rate: f64, round_trip: f64) -> Path {
            Path {
                rate,
                down: round_trip / 2.0,
                up: round_trip / 2.0,
                free_at: 0.0,
                worst: 0.0,
                sent: 0,
                jitter: 0.0,
                draw: 1,
                out: VecDeque::new(),
                last_out: 0.0,
                back: VecDeque::new(),
                last_back: 0.0,
                tick: 0.0,
                acked: f64::NEG_INFINITY,
                fresh: f64::NEG_INFINITY,
                stamping: true,
                drift: 0.0,
            }
        }

        /// Up to `jitter` seconds, drawn afresh each time.
        fn held(&mut self) -> f64 {
            self.draw = self
                .draw
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1_442_695_040_888_963_407);
            self.jitter * ((self.draw >> 33) % 1000) as f64 / 1000.0
        }

        fn write(&mut self, flight: &mut Flight, len: usize, now: f64, bulk: bool) {
            let start = self.free_at.max(now);
            self.free_at = start + len as f64 / self.rate;
            self.worst = self.worst.max(self.free_at - now);
            self.sent += len as u64;
            let ping = flight.wrote(len, now, bulk, false);
            if bulk || ping.is_some() {
                self.send(ping, bulk);
            }
        }

        /// A ping written at `now` with nothing ahead of it, as while a frame is held.
        fn ping(&mut self, ping: Vec<u8>, now: f64) {
            self.free_at = self.free_at.max(now);
            self.send(Some(ping), false);
        }

        fn send(&mut self, ping: Option<Vec<u8>>, bulk: bool) {
            let at = (self.free_at + self.down + self.held()).max(self.last_out);
            self.last_out = at;
            self.out.push_back((at, ping, bulk));
        }

        fn back(&mut self, at: f64, what: Back) {
            let at = (at + self.up + self.held()).max(self.last_back);
            self.last_back = at;
            self.back.push_back((at, what));
        }

        /// What reached the page by `now`, what its timer did, and what came back.
        fn deliver(&mut self, flight: &mut Flight, now: f64) {
            if self.tick == 0.0 {
                self.tick = (now / PAGE_TICK).ceil() * PAGE_TICK;
            }
            loop {
                let arrival = self.out.front().map(|o| o.0).filter(|at| *at <= now);
                let tick = (self.tick <= now).then_some(self.tick);
                match (arrival, tick) {
                    (Some(at), tick) if tick.is_none_or(|tick| at <= tick) => {
                        let (at, ping, bulk) = self.out.pop_front().unwrap();
                        if bulk {
                            self.fresh = at;
                        }
                        if let Some(ping) = ping {
                            self.back(at, Back::Pong(ping));
                        }
                    }
                    (_, Some(tick)) => {
                        self.tick += PAGE_TICK;
                        let due = tick - self.fresh < PAGE_HEARTBEAT
                            || tick - self.acked >= PAGE_HEARTBEAT;
                        if self.stamping && self.fresh.is_finite() && due {
                            self.acked = tick;
                            let stamp = tick * (1.0 + self.drift) + PAGE_CLOCK;
                            self.back(tick, Back::Stamp(stamp));
                        }
                    }
                    _ => break,
                }
            }
            while self.back.front().is_some_and(|(at, _)| *at <= now) {
                match self.back.pop_front().unwrap() {
                    (at, Back::Pong(ping)) => assert!(flight.note_pong(&ping, at)),
                    (at, Back::Stamp(stamp)) => flight.note_stamp(stamp, at),
                }
            }
        }
    }

    /// A stamp the page sent at `now` that took no time back.
    fn stamp(flight: &mut Flight, now: f64) {
        flight.note_stamp(now + PAGE_CLOCK, now);
    }

    struct Run {
        /// The most a byte waited in the path's queue once the narrowed path had settled.
        worst: f64,
        /// Frames left waiting with their sender at the end.
        waiting: usize,
        /// Bytes the path took over the last second.
        last_second: u64,
        /// For how long the frames were paced, in all.
        paced: f64,
    }

    /// A stream of `frame`-byte frames every `every` seconds for `seconds`, each written when
    /// the gauge lets it (or at once without one), over `path`, whose rate becomes `narrow`
    /// at `at`; the queue is read from `settle` after that on.
    fn stream(
        gauged: bool,
        path: &mut Path,
        frame: usize,
        every: f64,
        seconds: f64,
        narrow: Option<(f64, f64)>,
        settle: f64,
    ) -> Run {
        let mut flight = Flight::default();
        let mut waiting = 0usize;
        let mut due = 10.0;
        let mut now = 10.0;
        let mut paced = 0.0;
        let mut before_last = 0;
        while now < 10.0 + seconds {
            if let Some((at, rate)) = narrow
                && now >= 10.0 + at
            {
                path.rate = rate;
                if now < 10.0 + at + settle {
                    path.worst = 0.0;
                }
            }
            if now < 10.0 + seconds - 1.0 {
                before_last = path.sent;
            }
            path.deliver(&mut flight, now);
            if now >= due {
                waiting += 1;
                due += every;
            }
            while waiting > 0 && (!gauged || flight.clear(now, FLIGHT_LATE_S, waiting > 1)) {
                path.write(&mut flight, frame, now, true);
                waiting -= 1;
            }
            if flight.paced() {
                paced += 0.001;
            }
            now += 0.001;
        }
        Run {
            worst: path.worst,
            waiting,
            last_second: path.sent - before_last,
            paced,
        }
    }

    /// A 60 fps stream at 8 Mbit/s for 20 s over a path that carries it (or that narrows to
    /// `narrow` bytes a second from 2 s to 14 s), whose way back holds pongs and stamps
    /// `held(t)` seconds longer `t` seconds into those twelve: the longest a frame waited
    /// to be written, how long frames were paced in all, and the most a byte waited in the
    /// path's queue from a second into the narrow stretch on.
    fn held_up_on_the_way_back(narrow: Option<f64>, held: impl Fn(f64) -> f64) -> (f64, f64, f64) {
        let mut flight = Flight::default();
        let mut path = Path::new(2_500_000.0, 0.02);
        let (mut due, mut now) = (10.0, 10.0);
        let mut queue: VecDeque<f64> = VecDeque::new();
        let (mut longest, mut paced) = (0.0f64, 0.0);
        while now < 30.0 {
            let t: f64 = now - 12.0;
            let trouble = (0.0..12.0).contains(&t);
            path.up = 0.01 + if trouble { held(t) } else { 0.0 };
            if let Some(narrow) = narrow {
                path.rate = if trouble { narrow } else { 2_500_000.0 };
                if (0.0..1.0).contains(&t) {
                    path.worst = 0.0;
                }
            }
            path.deliver(&mut flight, now);
            if now >= due {
                queue.push_back(now);
                due += 1.0 / 60.0;
            }
            while !queue.is_empty() && flight.clear(now, FLIGHT_LATE_S, queue.len() > 1) {
                path.write(&mut flight, 16_384, now, true);
                longest = longest.max(now - queue.pop_front().unwrap());
            }
            if !queue.is_empty()
                && let Some(ping) = flight.probe_held(now)
            {
                path.ping(ping, now);
            }
            if flight.paced() {
                paced += 0.001;
            }
            now += 0.001;
        }
        (longest, paced, path.worst)
    }

    /// Ways back that hold the page's pongs and stamps up: an upload filling the uplink's
    /// buffer to 1.2 s, a shorter one filling in a fifth of a second, a hop that lets what
    /// it holds go at once every 0.42 s and every 0.2 s, a queue that builds and empties,
    /// and a way back 150 ms longer.
    const WAYS_BACK: [fn(f64) -> f64; 6] = [
        |t| (t / 2.0).min(1.0) * 1.2,
        |t| (t / 0.2).min(1.0) * 0.4,
        |t| 0.42 - (t % 0.42),
        |t| 0.2 - (t % 0.2),
        |t| 0.02 + 0.13 * (t / 0.4).fract(),
        |_| 0.15,
    ];

    #[test]
    fn what_holds_pongs_up_on_their_way_back_holds_no_video() {
        // The pings stand out for up to 1.2 s past the least round trip, and every stamp
        // that arrives says the page had answered all that reached it: nothing is paced and
        // no frame waits.
        for (k, held) in WAYS_BACK.into_iter().enumerate() {
            let (longest, paced, _) = held_up_on_the_way_back(None, held);
            assert!(paced == 0.0 && longest < 0.002, "{k}: {paced} {longest}");
        }
    }

    #[test]
    fn a_narrowed_path_is_paced_whatever_holds_its_pongs_up() {
        // The same ways back behind a path narrowed to 1.5 Mbit/s: what the page says comes
        // that much later, and the queue stands that much deeper, where the path left to
        // itself holds seconds.
        let (_, _, steady) = held_up_on_the_way_back(Some(187_500.0), |_| 0.0);
        assert!(steady < 0.2, "{steady}");
        for (k, held) in WAYS_BACK.into_iter().enumerate() {
            let (_, paced, worst) = held_up_on_the_way_back(Some(187_500.0), held);
            assert!(paced > 11.0, "{k}: {paced}");
            assert!(worst < 0.7, "{k}: {worst}");
        }
    }

    #[test]
    fn a_way_back_that_grew_longer_over_a_still_screen_holds_no_video() {
        // The stream runs, the screen stands still for twelve minutes with the page's
        // heartbeat acks alone coming in, and the way back grows 150 ms longer meanwhile.
        // The stream that starts again finds its pongs that much later: its round trips'
        // least and its stamps' least were both read before, so the stamps still say where
        // the pongs stand, and nothing is paced.
        let mut flight = Flight::default();
        let mut path = Path::new(2_500_000.0, 0.02);
        let (mut due, mut now) = (10.0, 10.0);
        let mut queue: VecDeque<f64> = VecDeque::new();
        let (mut longest, mut paced) = (0.0f64, 0.0);
        while now < 760.0 {
            let still = (15.0..735.0).contains(&now);
            if now >= 60.0 {
                path.up = 0.16;
            }
            path.deliver(&mut flight, now);
            if now >= due {
                if !still {
                    queue.push_back(now);
                }
                due += 1.0 / 60.0;
            }
            while !queue.is_empty() && flight.clear(now, FLIGHT_LATE_S, queue.len() > 1) {
                path.write(&mut flight, 16_384, now, true);
                longest = longest.max(now - queue.pop_front().unwrap());
            }
            if flight.paced() {
                paced += 0.001;
            }
            now += 0.001;
        }
        assert!(paced == 0.0 && longest < 0.002, "{paced} {longest}");
    }

    /// A 60 fps stream of 16 KiB frames over a path narrowed to 1.5 Mbit/s at 12 s, with one
    /// key frame of `key` bytes once paced from 14 s on: when the gauge stopped pacing in
    /// the `seconds` after it, if it did, and the most frames it let go at one moment.
    fn key_frame_over_a_narrowed_path(key: usize, seconds: f64) -> (Option<f64>, usize) {
        let mut flight = Flight::default();
        let mut path = Path::new(2_500_000.0, 0.02);
        let (mut waiting, mut due, mut now) = (0usize, 10.0, 10.0);
        let mut key_at: Option<f64> = None;
        let (mut burst, mut unpaced_at) = (0usize, None);
        while now < 16.0 + seconds {
            if now >= 12.0 {
                path.rate = 187_500.0;
            }
            path.deliver(&mut flight, now);
            if now >= due {
                waiting += 1;
                due += 1.0 / 60.0;
            }
            let mut wrote = 0usize;
            while waiting > 0 && flight.clear(now, FLIGHT_LATE_S, waiting > 1) {
                let len = if key_at.is_none() && now >= 14.0 && flight.paced() {
                    key_at = Some(now);
                    key
                } else {
                    16_384
                };
                path.write(&mut flight, len, now, true);
                waiting -= 1;
                wrote += 1;
            }
            if waiting > 0
                && let Some(ping) = flight.probe_held(now)
            {
                path.ping(ping, now);
            }
            if let Some(sent) = key_at
                && now > sent
                && now - sent < seconds
            {
                burst = burst.max(wrote);
                if !flight.paced() && unpaced_at.is_none() {
                    unpaced_at = Some(now - sent);
                }
            }
            now += 0.001;
        }
        assert!(key_at.is_some(), "the key frame went");
        (unpaced_at, burst)
    }

    #[test]
    fn a_key_frame_slower_than_a_stamp_grows_old_keeps_the_pacing() {
        // 500 kB at 1.5 Mbit/s takes 2.7 s to cross, and 1.5 MB takes 8 s: no pong comes
        // meanwhile, and the page, which gets no frame either, stamps its acks on. It is
        // not a peer that stopped answering: the frames behind the key frame wait, every
        // ping behind it is answered once it has crossed (the path's page takes no pong the
        // gauge has forgotten), and nothing goes at once.
        for (key, seconds) in [(300_000, 3.0), (500_000, 4.0), (1_500_000, 9.5)] {
            let (unpaced_at, burst) = key_frame_over_a_narrowed_path(key, seconds);
            assert!(
                unpaced_at.is_none() && burst <= 2,
                "{key}: {unpaced_at:?} {burst}"
            );
        }
    }

    #[test]
    fn a_fast_page_clock_behind_a_way_back_held_for_minutes_reads_late_for_moments() {
        // A page whose clock runs 200 parts in a million fast, behind a way back an upload
        // holds half a second from 30 s on without a break, on a path that carries the
        // stream. The stamps' least stays one from before the hold, the clock gains on it,
        // and once the gain passes the bound (500 s on) the path reads late: for moments,
        // each ended by the pace it kept, and no frame waits long.
        let mut flight = Flight::default();
        let mut path = Path::new(2_500_000.0, 0.02);
        path.drift = 200e-6;
        let (mut due, mut now) = (10.0, 10.0);
        let mut queue: VecDeque<f64> = VecDeque::new();
        let (mut longest, mut paced, mut first) = (0.0f64, 0.0, None);
        while now < 700.0 {
            path.up = if now >= 30.0 { 0.51 } else { 0.01 };
            path.deliver(&mut flight, now);
            if now >= due {
                queue.push_back(now);
                due += 1.0 / 60.0;
            }
            while !queue.is_empty() && flight.clear(now, FLIGHT_LATE_S, queue.len() > 1) {
                path.write(&mut flight, 16_384, now, true);
                longest = longest.max(now - queue.pop_front().unwrap());
            }
            if !queue.is_empty()
                && let Some(ping) = flight.probe_held(now)
            {
                path.ping(ping, now);
            }
            if flight.paced() {
                paced += 0.001;
                first.get_or_insert(now);
            }
            now += 0.001;
        }
        assert!(first.is_some_and(|at| at > 500.0), "{first:?}");
        assert!(paced < 20.0 && longest < 0.3, "{paced} {longest} {first:?}");
    }

    #[test]
    fn a_page_that_stamps_nothing_is_not_judged() {
        // Its pings stand out as a narrowed path's do, and nothing says they are not held
        // up on their way back: the stream is written as it comes.
        let narrow = Some((2.0, 187_500.0));
        let mut ungauged = Path::new(2_500_000.0, 0.02);
        let left = stream(false, &mut ungauged, 16_384, 1.0 / 60.0, 5.0, narrow, 0.5);
        let mut path = Path::new(2_500_000.0, 0.02);
        path.stamping = false;
        let run = stream(true, &mut path, 16_384, 1.0 / 60.0, 5.0, narrow, 0.5);
        assert_eq!(run.waiting, 0);
        assert_eq!(run.last_second, left.last_second);
        // One whose stamps stop is judged no more once the last is `FLIGHT_DISTRUST_S` old.
        let mut flight = Flight::default();
        let mut path = Path::new(2_500_000.0, 0.02);
        let (mut waiting, mut due, mut now) = (0usize, 10.0, 10.0);
        while now < 17.0 {
            if now >= 12.0 {
                path.rate = 187_500.0;
            }
            path.stamping = now < 14.0;
            path.deliver(&mut flight, now);
            if now >= due {
                waiting += 1;
                due += 1.0 / 60.0;
            }
            while waiting > 0 && flight.clear(now, FLIGHT_LATE_S, waiting > 1) {
                path.write(&mut flight, 16_384, now, true);
                waiting -= 1;
            }
            if (13.9..13.901).contains(&now) {
                assert!(flight.paced() && waiting > 50, "{waiting}");
            }
            now += 0.001;
        }
        assert!(!flight.paced());
        assert_eq!(waiting, 0);
    }

    #[test]
    fn a_path_that_carries_the_stream_is_never_paced() {
        // 8 Mbit/s of 16 KiB frames over 20 Mbit/s, on a short path and on a long one.
        for round_trip in [0.002, 0.08, 0.3] {
            let mut path = Path::new(2_500_000.0, round_trip);
            let run = stream(true, &mut path, 16_384, 1.0 / 60.0, 5.0, None, 0.0);
            assert_eq!(run.waiting, 0, "{round_trip}");
            assert!(run.worst < 0.02, "{round_trip}: {}", run.worst);
            assert!(run.paced == 0.0, "{round_trip}: {}", run.paced);
        }
    }

    #[test]
    fn a_narrowed_path_holds_about_what_it_delivers_in_the_bound() {
        // The same stream, the path narrowed to 1.5 Mbit/s two seconds in. Without the gauge
        // everything written stands in the path; with it the path holds about the bound once
        // what was written before the path showed late has drained, and still takes all it
        // carries.
        let narrow = Some((2.0, 187_500.0));
        let mut ungauged = Path::new(2_500_000.0, 0.02);
        let run = stream(false, &mut ungauged, 16_384, 1.0 / 60.0, 5.0, narrow, 0.5);
        assert!(run.worst > 5.0, "{}", run.worst);
        for round_trip in [0.002, 0.02, 0.1, 0.3] {
            let mut path = Path::new(2_500_000.0, round_trip);
            let run = stream(true, &mut path, 16_384, 1.0 / 60.0, 9.0, narrow, 3.0);
            assert!(
                run.worst < 0.1 + FLIGHT_LATE_S.max(round_trip),
                "{round_trip}: {}",
                run.worst
            );
            assert!(run.waiting > 100, "{round_trip}: {}", run.waiting);
            assert!(
                run.last_second > 160_000,
                "{round_trip}: the path stays in use: {}",
                run.last_second
            );
        }
    }

    #[test]
    fn a_stream_the_narrowed_path_still_carries_is_not_paced() {
        // 1 Mbit/s of 2 KiB frames over a path narrowed to 1.5 Mbit/s: nothing waits, at
        // no moment.
        let mut path = Path::new(2_500_000.0, 0.02);
        let narrow = Some((2.0, 187_500.0));
        let run = stream(true, &mut path, 2_048, 1.0 / 60.0, 6.0, narrow, 0.5);
        assert_eq!(run.waiting, 0);
        assert!(run.worst < 0.1, "{}", run.worst);
        assert!(run.paced == 0.0, "{}", run.paced);
    }

    #[test]
    fn a_stream_that_falls_under_the_narrowed_rate_leaves_the_pacing() {
        // 8 Mbit/s of 16 KiB frames over a path narrowed to 1.5 Mbit/s at 12 s is paced. At
        // 15 s what waits is let go of (as the gate does) and the stream comes at 1 Mbit/s,
        // which the path carries: the pacing ends with the first frame that finds none in
        // flight and none behind it, and does not come back.
        let mut flight = Flight::default();
        let mut path = Path::new(2_500_000.0, 0.02);
        let (mut waiting, mut due, mut now) = (0usize, 10.0, 10.0);
        let (mut paced_before, mut left_at, mut paced_after) = (0.0, None, 0.0);
        while now < 20.0 {
            if now >= 12.0 {
                path.rate = 187_500.0;
            }
            if (15.0..15.001).contains(&now) {
                waiting = 0;
            }
            path.deliver(&mut flight, now);
            if now >= due {
                waiting += 1;
                due += 1.0 / 60.0;
            }
            while waiting > 0 && flight.clear(now, FLIGHT_LATE_S, waiting > 1) {
                let len = if now < 15.0 { 16_384 } else { 2_048 };
                path.write(&mut flight, len, now, true);
                waiting -= 1;
            }
            match (now < 15.0, flight.paced()) {
                (true, true) => paced_before += 0.001,
                (false, false) if left_at.is_none() => left_at = Some(now - 15.0),
                (false, true) if left_at.is_some() => paced_after += 0.001,
                _ => {}
            }
            now += 0.001;
        }
        assert!(paced_before > 2.5, "{paced_before}");
        assert!(left_at.is_some_and(|at| at < 0.5), "{left_at:?}");
        assert!(
            paced_after == 0.0 && waiting == 0,
            "{paced_after} {waiting}"
        );
    }

    /// The stream of `a_path_that_carries_the_stream_is_never_paced` over a path of
    /// `round_trip` that `trouble` changes at two seconds: how long after `back` the last
    /// frame that waited was written.
    fn caught_up_after(round_trip: f64, back: f64, trouble: impl Fn(&mut Path, f64)) -> f64 {
        let mut flight = Flight::default();
        let mut path = Path::new(2_500_000.0, round_trip);
        let (mut waiting, mut due, mut now, mut waited_at) = (0usize, 10.0, 10.0, 10.0);
        while now < 10.0 + back + 8.0 {
            trouble(&mut path, now - 10.0);
            path.deliver(&mut flight, now);
            if now >= due {
                waiting += 1;
                due += 1.0 / 60.0;
            }
            while waiting > 0 && flight.clear(now, FLIGHT_LATE_S, waiting > 1) {
                path.write(&mut flight, 16_384, now, true);
                waiting -= 1;
            }
            if waiting > 0 {
                waited_at = now;
            }
            now += 0.001;
        }
        waited_at - 10.0 - back
    }

    #[test]
    fn a_path_that_stood_still_a_moment_takes_what_waited_at_once() {
        // Nothing crosses for a quarter of a second (a radio's pause), then the path is
        // back: what waited meanwhile is through within the pause again.
        for round_trip in [0.002, 0.02, 0.1, 0.3] {
            let after = caught_up_after(round_trip, 2.25, |path, at| {
                if (2.0..2.001).contains(&at) {
                    path.free_at = path.free_at.max(10.0 + at) + 0.25;
                }
            });
            assert!(after < 0.25 + 2.0 * round_trip, "{round_trip}: {after}");
        }
    }

    #[test]
    fn a_path_that_comes_back_takes_its_backlog_at_its_rate() {
        // Narrowed to 1.5 Mbit/s for a second, then back at 20: the megabyte that waited
        // goes at what the path now carries, the gauge finding that rate within a few
        // round trips.
        for round_trip in [0.002, 0.02, 0.1, 0.3] {
            let after = caught_up_after(round_trip, 3.0, |path, at| {
                path.rate = if (2.0..3.0).contains(&at) {
                    187_500.0
                } else {
                    2_500_000.0
                };
            });
            assert!(after < 2.0 + 8.0 * round_trip, "{round_trip}: {after}");
        }
    }

    #[test]
    fn a_longer_way_round_is_not_taken_for_a_narrowed_path() {
        // The round trip steps from 27 ms to 207 ms, by the way there, by the way back, or
        // by both, and the path carries what it did: the pings stand out past the bound, the
        // pongs then show the stream arriving as it was sent, and nothing waits for long.
        for (down, up) in [(0.1035, 0.1035), (0.1935, 0.0135), (0.0135, 0.1935)] {
            let after = caught_up_after(0.027, 2.0, |path, at| {
                if at >= 2.0 {
                    (path.down, path.up) = (down, up);
                }
            });
            assert!(after < 0.6, "{down}/{up}: {after}");
        }
    }

    #[test]
    fn a_path_a_little_under_the_stream_stays_paced_at_its_least_round_trip() {
        // The path carries 85% of the stream from two seconds in: under what counts as
        // keeping up, so the frames are paced, the queue stays about the bound, and the
        // least round trip is not moved onto the queue the stream itself keeps standing.
        let mut flight = Flight::default();
        let mut path = Path::new(2_500_000.0, 0.02);
        let (mut waiting, mut due, mut now) = (0usize, 10.0, 10.0);
        while now < 18.0 {
            if now >= 12.0 {
                path.rate = 0.85 * 16_384.0 * 60.0;
                if now < 13.0 {
                    path.worst = 0.0;
                }
            }
            path.deliver(&mut flight, now);
            if now >= due {
                waiting += 1;
                due += 1.0 / 60.0;
            }
            while waiting > 0 && flight.clear(now, FLIGHT_LATE_S, waiting > 1) {
                path.write(&mut flight, 16_384, now, true);
                waiting -= 1;
            }
            now += 0.001;
        }
        assert!(path.worst < 0.25, "{}", path.worst);
        assert!(flight.floor().unwrap() < 0.05, "{:?}", flight.floor());
        assert!(waiting > 30, "{waiting}");
    }

    #[test]
    fn a_stream_that_resumes_over_the_narrowed_path_is_paced_again() {
        // The path narrows, the frames that waited are let go and none is offered for a
        // while (the gate), then the stream is back at its old rate over the path as
        // narrow as before: it is paced again, and the path's least round trip stays put.
        let mut flight = Flight::default();
        let mut path = Path::new(2_500_000.0, 0.02);
        let (mut waiting, mut due, mut now) = (0usize, 10.0, 10.0);
        while now < 19.0 {
            if now >= 12.0 {
                path.rate = 500_000.0;
            }
            if (13.5..14.1).contains(&now) {
                waiting = 0;
                due = 14.1;
            }
            if (15.0..15.001).contains(&now) {
                path.worst = 0.0;
            }
            path.deliver(&mut flight, now);
            if now >= due {
                waiting += 1;
                due += 1.0 / 60.0;
            }
            while waiting > 0 && flight.clear(now, FLIGHT_LATE_S, waiting > 1) {
                path.write(&mut flight, 16_384, now, true);
                waiting -= 1;
            }
            now += 0.001;
        }
        assert!(path.worst < 0.25, "{}", path.worst);
        assert!(flight.floor().unwrap() < 0.05, "{:?}", flight.floor());
        assert!(flight.paced());
    }

    #[test]
    fn a_stream_that_resumes_after_a_pause_is_not_held_to_the_rate_of_the_pause() {
        // Paced on a narrowed path with frames waiting, then nothing but audio for two
        // seconds (the gate let the video go), then a ten-chunk frame with more behind it
        // over the path, wide again: the chunks go at once.
        let mut flight = Flight::default();
        let mut path = Path::new(2_500_000.0, 0.02);
        let (mut waiting, mut due, mut now) = (0usize, 10.0, 10.0);
        while now < 12.0 {
            if now >= 11.0 {
                path.rate = 187_500.0;
            }
            path.deliver(&mut flight, now);
            if now >= due {
                waiting += 1;
                due += 1.0 / 60.0;
            }
            while waiting > 0 && flight.clear(now, FLIGHT_LATE_S, waiting > 1) {
                path.write(&mut flight, 16_384, now, true);
                waiting -= 1;
            }
            now += 0.001;
        }
        assert!(flight.paced() && waiting > 10);
        while now < 14.0 {
            path.deliver(&mut flight, now);
            path.write(&mut flight, 500, now, false);
            now += 0.01;
        }
        path.rate = 2_500_000.0;
        let mut chunks = 10;
        let from = now;
        while chunks > 0 {
            path.deliver(&mut flight, now);
            if flight.clear(now, FLIGHT_LATE_S, true) {
                path.write(&mut flight, 8_192, now, true);
                chunks -= 1;
            }
            now += 0.001;
        }
        assert!(now - from < 0.05, "{}", now - from);
    }

    #[test]
    fn a_pong_answers_the_pings_before_its_own() {
        let mut flight = Flight::default();
        stamp(&mut flight, 10.0);
        let first = flight.wrote(1000, 10.0, true, false).unwrap();
        let second = flight.wrote(1000, 10.03, true, false).unwrap();
        let third = flight.wrote(1000, 10.06, true, false).unwrap();
        assert!(flight.note_pong(&third, 10.08));
        assert_eq!(flight.in_flight(), 0);
        assert!(!flight.note_pong(&first, 10.09));
        assert!(!flight.note_pong(&second, 10.09));
        // A ping written while a frame is held does the same for one that went unanswered.
        flight.wrote(16_384, 10.2, true, false).unwrap();
        assert!(flight.probe_held(10.25).is_none(), "within the gap");
        let held = flight.probe_held(10.31).unwrap();
        stamp(&mut flight, 10.35);
        assert!(!flight.clear(10.35, FLIGHT_LATE_S, true));
        assert!(flight.note_pong(&held, 10.36));
        assert!(flight.clear(10.36, FLIGHT_LATE_S, true));
    }

    #[test]
    fn striped_frames_and_small_ones_are_paced_as_whole_frames_are() {
        // Six chunks to the frame, and a thousand small frames a second, over the narrowed
        // path: the queue stays about the bound and the path stays in use.
        let narrow = Some((2.0, 187_500.0));
        for (frame, every) in [(2_731, 1.0 / 360.0), (1_024, 0.001)] {
            let mut path = Path::new(2_500_000.0, 0.02);
            let run = stream(true, &mut path, frame, every, 9.0, narrow, 3.0);
            assert!(run.worst < 0.25, "{frame}: {}", run.worst);
            assert!(run.last_second > 160_000, "{frame}: {}", run.last_second);
        }
    }

    #[test]
    fn a_held_frame_goes_once_no_bulk_frame_is_in_flight() {
        // A path of 150 ms with sparse video: a frame's pong is held up, the next frame
        // finds the path late and waits, pings going out meanwhile. It goes as soon as a
        // pong says the frame before it arrived, whatever ping is still out and with no
        // rate ever read.
        let mut flight = Flight::default();
        stamp(&mut flight, 10.0);
        let first = flight.wrote(1000, 10.0, true, false).unwrap();
        assert!(flight.note_pong(&first, 10.15));
        let frame = flight.wrote(16_384, 11.0, true, false).unwrap();
        stamp(&mut flight, 11.26);
        assert!(!flight.clear(11.26, FLIGHT_LATE_S, false));
        let held = flight.probe_held(11.26).unwrap();
        assert!(flight.note_pong(&frame, 11.30));
        assert!(flight.clear(11.30, FLIGHT_LATE_S, false));
        assert!(!flight.paced());
        assert!(flight.note_pong(&held, 11.41));
    }

    #[test]
    fn a_burst_the_path_drains_does_not_move_its_least_round_trip() {
        // 1 Mbit/s of 2 KiB frames over 1.5 Mbit/s, and one frame of 100 KiB among them:
        // the frames behind it come back closer together than they went out (the path is
        // draining what the large frame put there), which is not a longer way round.
        let mut flight = Flight::default();
        let mut path = Path::new(187_500.0, 0.02);
        let (mut waiting, mut due, mut now, mut paced) = (0usize, 10.0, 10.0, 0.0);
        let mut large = true;
        while now < 14.0 {
            path.deliver(&mut flight, now);
            if now >= due {
                waiting += 1;
                due += 1.0 / 60.0;
            }
            while waiting > 0 && flight.clear(now, FLIGHT_LATE_S, waiting > 1) {
                let len = if large && now >= 11.0 { 102_400 } else { 2_048 };
                large = large && now < 11.0;
                path.write(&mut flight, len, now, true);
                waiting -= 1;
            }
            if flight.paced() {
                paced += 0.001;
            }
            now += 0.001;
        }
        // Paced for a while, the one state in which the pace kept could move the floor.
        assert!(paced > 0.5, "{paced}");
        assert!(flight.floor().unwrap() < 0.05, "{:?}", flight.floor());
        assert_eq!(waiting, 0);
        assert!(!flight.paced());
    }

    #[test]
    fn a_longer_way_round_the_pongs_could_not_show_becomes_the_floor_in_time() {
        // A frame every 0.4 s and a round trip that steps from 20 ms to 500 ms, half of it
        // each way: one ping is out at a time, too few for their pongs to show the pace
        // kept. The stamps tell the way back's share at once; the way out's holds every
        // frame until the one before it arrived, and after the settle span the least seen
        // is the path's and nothing waits any more.
        let mut flight = Flight::default();
        let mut path = Path::new(2_500_000.0, 0.02);
        let (mut waiting, mut due, mut now) = (0usize, 10.0, 10.0);
        let mut waited_at = 10.0;
        while now < 22.0 {
            if now >= 12.0 {
                (path.down, path.up) = (0.25, 0.25);
            }
            path.deliver(&mut flight, now);
            if now >= due {
                waiting += 1;
                due += 0.4;
            }
            while waiting > 0 && flight.clear(now, FLIGHT_LATE_S, waiting > 1) {
                path.write(&mut flight, 16_384, now, true);
                waiting -= 1;
            }
            if waiting > 0 {
                waited_at = now;
            }
            now += 0.001;
        }
        assert!(
            (flight.floor().unwrap() - 0.26).abs() < 0.02,
            "{:?}",
            flight.floor()
        );
        assert!(waited_at < 12.0 + FLIGHT_SETTLE_S + 1.5, "{waited_at}");
    }

    #[test]
    fn pings_written_as_the_socket_pushed_back_say_nothing_of_the_pace_kept() {
        // The same pongs as a longer way round gives, of pings that went out behind writes
        // the socket made wait: they were spaced by what the path took, and vouch for
        // nothing.
        for (clocked, kept) in [(false, true), (true, false)] {
            let mut flight = Flight::default();
            stamp(&mut flight, 10.0);
            let first = flight.wrote(1000, 10.0, true, false).unwrap();
            assert!(flight.note_pong(&first, 10.02));
            let pings: Vec<_> = (0..6)
                .map(|k| {
                    let at = 10.1 + f64::from(k) * 0.03;
                    flight.wrote(16_384, at, true, clocked).unwrap()
                })
                .collect();
            stamp(&mut flight, 10.3);
            assert!(!flight.clear(10.3, FLIGHT_LATE_S, true));
            assert!(flight.paced());
            for (k, ping) in pings.iter().enumerate() {
                assert!(flight.note_pong(ping, 10.32 + k as f64 * 0.03));
            }
            assert_eq!(!flight.paced(), kept, "clocked {clocked}");
        }
    }

    #[test]
    fn a_path_that_delivers_unevenly_is_not_late_for_it() {
        // 8 Mbit/s over 20 Mbit/s with a round trip of 40 ms and up to 150 ms more each way,
        // piece by piece: the round trips run 300 ms past the least of them, the slack grows
        // with how far apart they lie, and the stream is not held to a queue it does not
        // stand in. The same path narrowed to 1.5 Mbit/s still is paced: over eight draws of
        // what each piece is held, to about a second of queue and never to two, the slack
        // and the bunched pongs' rates allowing that much, where the path left to itself
        // holds three and more.
        let mut worst = Vec::new();
        for seed in 1..=8u64 {
            let mut path = Path::new(2_500_000.0, 0.04);
            path.jitter = 0.15;
            path.draw = seed * 7919;
            let mut flight = Flight::default();
            let (mut waiting, mut due, mut now) = (0usize, 10.0, 10.0);
            let (mut held_ms, mut most) = (0usize, 0usize);
            while now < 30.0 {
                if now >= 20.0 {
                    path.rate = 187_500.0;
                }
                path.deliver(&mut flight, now);
                if now >= due {
                    waiting += 1;
                    due += 1.0 / 60.0;
                }
                while waiting > 0 && flight.clear(now, FLIGHT_LATE_S, waiting > 1) {
                    path.write(&mut flight, 16_384, now, true);
                    waiting -= 1;
                }
                if (13.0..20.0).contains(&now) {
                    held_ms += usize::from(waiting > 1);
                    most = most.max(waiting);
                }
                if (23.0..23.001).contains(&now) {
                    path.worst = 0.0;
                }
                now += 0.001;
            }
            assert!(
                held_ms < 700,
                "{seed}: {held_ms} ms of 7 s with frames waiting"
            );
            assert!(most <= 12, "{seed}: {most}");
            assert!(waiting > 100, "{seed}: {waiting}");
            worst.push(path.worst);
        }
        worst.sort_by(f64::total_cmp);
        assert!(worst[4] < 1.2 && worst[7] < 2.0, "{worst:?}");
    }

    #[test]
    fn a_frame_slow_to_cross_a_narrow_path_is_not_a_longer_way_round() {
        // The path narrowed to 1 Mbit/s, where one 16 KiB frame takes 131 ms to cross, past
        // the bound: every frame goes alone and its ping stands past the line, but the pings
        // written while the next frame waits come back early, the floor stays, and so does
        // the pacing.
        let mut flight = Flight::default();
        let mut path = Path::new(2_500_000.0, 0.02);
        let (mut waiting, mut due, mut now, mut probed) = (0usize, 10.0, 10.0, 0usize);
        while now < 22.0 {
            if now >= 12.0 {
                path.rate = 125_000.0;
            }
            if (14.0..14.001).contains(&now) {
                path.worst = 0.0;
            }
            path.deliver(&mut flight, now);
            if now >= due {
                waiting += 1;
                due += 1.0 / 60.0;
            }
            while waiting > 0 && flight.clear(now, FLIGHT_LATE_S, waiting > 1) {
                path.write(&mut flight, 16_384, now, true);
                waiting -= 1;
            }
            if waiting > 0
                && let Some(ping) = flight.probe_held(now)
            {
                path.ping(ping, now);
                probed += 1;
            }
            now += 0.001;
        }
        assert!(probed > 20, "{probed}");
        assert!(flight.floor().unwrap() < 0.05, "{:?}", flight.floor());
        assert!(flight.paced());
        assert!(path.worst < 0.3, "{}", path.worst);
    }

    #[test]
    fn a_key_frame_holds_the_frames_behind_it_only_until_it_arrived() {
        // 300 KiB at once over 20 Mbit/s crosses in 120 ms, past the bound: the frames behind
        // it wait for its pong and then go at the path's rate.
        let mut flight = Flight::default();
        let mut path = Path::new(2_500_000.0, 0.02);
        // The page stamps once a first frame has reached it, and is pinged from then on.
        path.write(&mut flight, 1000, 9.8, true);
        path.deliver(&mut flight, 9.95);
        path.write(&mut flight, 1000, 9.95, true);
        path.deliver(&mut flight, 10.0);
        path.write(&mut flight, 1000, 10.0, true);
        path.deliver(&mut flight, 10.05);
        path.write(&mut flight, 307_200, 10.05, true);
        assert!(flight.clear(10.06, FLIGHT_LATE_S, true));
        path.write(&mut flight, 16_384, 10.06, true);
        path.deliver(&mut flight, 10.1);
        assert!(flight.clear(10.1, FLIGHT_LATE_S, true), "not late yet");
        path.deliver(&mut flight, 10.18);
        assert!(
            !flight.clear(10.18, FLIGHT_LATE_S, true),
            "late, no rate yet"
        );
        path.deliver(&mut flight, 10.25);
        assert!(flight.clear(10.25, FLIGHT_LATE_S, true));
        assert!(flight.rate(10.25).unwrap() > 1_000_000.0);
    }

    #[test]
    fn only_bulk_frames_are_pinged_and_audio_counts_in_flight() {
        let mut flight = Flight::default();
        assert!(
            flight.wrote(16_384, 9.9, true, false).is_none(),
            "no ping to a page that has stamped nothing"
        );
        stamp(&mut flight, 9.95);
        let first = flight.wrote(16_384, 9.95, true, false).unwrap();
        assert!(flight.note_pong(&first, 9.96));
        assert!(flight.wrote(500, 10.0, false, false).is_none());
        let ping = flight.wrote(16_384, 10.0, true, false).unwrap();
        assert!(
            flight.wrote(16_384, 10.01, true, false).is_none(),
            "within the gap"
        );
        assert_eq!(flight.in_flight(), 500 + 2 * 16_384);
        assert!(flight.note_pong(&ping, 10.02));
        assert!(!flight.note_pong(&ping, 10.03), "answered once");
        assert!(!flight.note_pong(b"12345678", 10.03));
        assert_eq!(flight.in_flight(), 16_384);
    }

    #[test]
    fn a_peer_that_answers_nothing_is_not_judged() {
        let mut flight = Flight::default();
        for k in 0..100 {
            let now = 10.0 + f64::from(k) * 0.05;
            flight.wrote(16_384, now, true, false);
            assert!(flight.clear(now, FLIGHT_LATE_S, true));
        }
        // One that answered and then stopped, its page stamping on, is held to its pings for
        // as long as a key frame may take to cross, and left alone after.
        let mut flight = Flight::default();
        stamp(&mut flight, 10.0);
        let ping = flight.wrote(1000, 10.0, true, false).unwrap();
        assert!(flight.note_pong(&ping, 10.01));
        flight.wrote(16_384, 11.0, true, false);
        for k in 0..18 {
            let now = 11.5 + f64::from(k) * 0.5;
            stamp(&mut flight, now);
            assert!(!flight.clear(now, FLIGHT_LATE_S, true), "{now}");
            assert!(flight.paced());
        }
        stamp(&mut flight, 11.0 + FLIGHT_UNANSWERED_S + 0.1);
        assert!(flight.clear(11.0 + FLIGHT_UNANSWERED_S + 0.1, FLIGHT_LATE_S, true));
        assert!(!flight.paced());
    }
}
