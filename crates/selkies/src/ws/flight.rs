/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! What a socket's path still holds of what its writer handed it, by the page's own word.
//!
//! The page counts the bytes of every binary message its socket hands it, and each ack it
//! sends says that count, its own clock as the ack leaves, the frame it last got and how
//! long ago (`note_stamp`): a stamp, on every tick of its ack timer while frames reach it.
//! The writer counts the same bytes as it writes them (`Flight::wrote`), so what it wrote
//! past the page's count is in flight, on its way through this host's send buffer, the
//! path's queues and whatever proxy stands between. The kernel's own count (`TCP_INFO`) ends
//! at the first proxy that terminates the connection, and a send buffer and a bottleneck's
//! queue together hold seconds of a stream once the path narrows, which every later byte,
//! audio among them, then waits behind.
//!
//! Nothing is added to the socket for it, either way: the acks are the ones the page sends
//! for the display's congestion steer, so the page's uplink carries what it carries without
//! a gauge, and an upload that fills it finds no more of the page's messages in its way.
//! That upload holds the acks up as it holds everything the page sends, and a stamp says by
//! how much: how far behind this clock it arrives, past the least any did, is how long its
//! way back held it, and taken off its arrival, that is when it left, on this clock. What
//! it says of the stream is as of then: the count the page had, and that the first bulk
//! frame past that count had not reached it, however long the stamp itself took.
//!
//! A path is late once the oldest bulk frame its page has not counted has been on its way
//! out, by the page's last stamp, `FLIGHT_LATE_S` past the path's least round trip, and past
//! that by `FLIGHT_JITTER_SLACK` times how far its round trips lay apart while nothing was
//! paced: a path that delivers unevenly is not late for it. That distance is read where a
//! round trip came down from the one before, as an uneven path's do as often as they go up:
//! the queue a stream builds under a path that narrowed only makes them longer, and must
//! not move the line it is about to cross. A round trip here is a frame's:
//! from its write to when the stamp says it arrived, which holds the way out, and of the
//! way back only the least it ever took. From then on bulk frames are paced to what the
//! path delivers: one goes once no bulk frame is in flight any more, or while what is in
//! flight would cross within the least round trip and `FLIGHT_LATE_S` (or as long again as
//! the round trip, on a path longer than that), what the way back holds, and the time since
//! the last stamp while the next is not due, at the rate the stamps show. Each stamp whose
//! count passed a frame gives a rate: what arrived between that frame and one that arrived
//! at least `FLIGHT_RATE_SPAN_S` before it, over the time between the two arrivals, which is
//! the path's rate while it had more to deliver and less while it stood idle; so the
//! largest of the last `FLIGHT_RATE_WINDOW_S` counts, and the rates read before the path
//! went late are let go. A frame the pacing holds waits in the
//! writer, and the frames behind it with their relay, where the display's gate still counts
//! them and can let them go, instead of in the path ahead of the audio written after them.
//!
//! The pacing is for a path that narrowed under the stream, and ends where that is not, or
//! no longer, so:
//! - with a bulk frame that finds none in flight, or the path on time and what is in flight
//!   fitting, and has no other waiting behind it (the stream is taking less than the path
//!   carries);
//! - once two frames written before the path showed late reach the page, after it did, as
//!   far apart as they were written, within `FLIGHT_KEEPS_UP`: the path carried those bytes
//!   at the pace they were written, neither slower (it is narrowed) nor faster (it is
//!   draining a queue the stream put there), so the frames stood out for a longer way round
//!   or a queue other traffic keeps, and the least round trip seen since is the path's from
//!   then on. The two frames lie at least so far apart that how far the path's round trips
//!   run apart is within the tenth a pace kept may be off by: an uneven path shows nothing
//!   over less. Only frames written while nothing was paced, and not as the socket made room,
//!   can say: what is written while the path is paced, or while the socket pushes back,
//!   goes out as it is delivered. And only frames that crossed at less than the path
//!   delivered when it was last paced, while it has shown no more since: a stream cut to
//!   the rate of a narrowed path keeps its pace behind a queue of its own, a key frame's,
//!   which is no longer way round;
//! - with a bulk frame that asks after none did for `FLIGHT_RATE_WINDOW_S` and finds none in
//!   flight (the stream had stopped);
//! - after `FLIGHT_PACED_MAX_S` on end, the least round trip of the stretch becoming the
//!   floor;
//! - once the page's last stamp is `FLIGHT_DISTRUST_S` old.
//!
//! Where two frames could not show the pace kept (too few out, a stall between them), a
//! longer way round still shows in the frames written with no bulk frame in flight: their
//! round trips hold the path's and that one frame's crossing, and no queue of the stream's.
//! Once `FLIGHT_SETTLE_FRAMES` of those stood past the late line through `FLIGHT_SETTLE_S`,
//! no more than `FLIGHT_SETTLE_GAP_S` apart and no frame's round trip coming within the line
//! meanwhile, the least of them is the floor; the pacing then ends by the rules above.
//!
//! A queue that builds slowly, under a path carrying most of the stream, is left to the
//! display's congestion steer, which reads it over its own windows.
//!
//! A page is never judged before a stamp of its has counted a bulk frame, nor while its
//! last stamp is `FLIGHT_DISTRUST_S` old, nor if it stamps nothing or says no count: an
//! older page, or a viewer's, which sends no ack. And only the stamps of its display's own
//! page are taken (`Client::note_stamp`'s caller): the gate that counts the frames held and
//! lets them go is that page's alone, and video held for another, a second tab's, would
//! only grow old with its relay. A count past what was written is not this socket's, and is
//! not taken.
//!
//! A page that stamps on is there, and counts what reaches it: a frame stands out behind
//! what is still crossing, a key frame over a narrowed path for seconds, and its video
//! stays paced. Only when its count has passed no bulk frame for `FLIGHT_UNANSWERED_S` is
//! it left to the kernel's own backpressure until it does again.
//!
//! The least a stamp arrived behind this clock is kept over the history the least round trip
//! is (`FloorHistory`), and read only while the page's count moves, as that one is, so a way
//! back that stays held is told from a path that grew longer for as long as either is
//! remembered, a still screen's pause included. A page whose clock runs slow against this
//! one reads as a way back held that much longer, a tenth of a second over that history at
//! the most for clocks 170 parts in a million apart, and its path is late that much later.
//! One whose clock runs fast is read as it is while its way back is unheld at times, each
//! stamp that arrives sooner becoming the least. Behind a way back held without a break for
//! minutes (a long upload) the least stays a stamp from before; a fast clock gains on it,
//! and the path reads late by that gain: past a tenth of a second, the same 170 parts in a
//! million over ten minutes, frames are paced for moments at a time, each ended by the pace
//! the path kept, until the history forgets that stamp. A clock that stepped, or a stamp
//! that was nonsense, shows in a frame that reached the page before it was written, and the
//! history starts over there.
//!
//! The page's count is its script's: what its engine holds and has not yet handed over reads
//! as on its way, so a page too busy to take its messages is paced as a late path is, and
//! let go as it catches up.

use std::collections::VecDeque;

use crate::ws::uplink::FloorHistory;

/// How far past the least round trip a frame stands on its way out before the path is late,
/// and how much of the path's delivery may stand in flight past the round trip while it is.
pub const FLIGHT_LATE_S: f64 = 0.1;
/// How far back the delivery rates a late path showed count.
pub const FLIGHT_RATE_WINDOW_S: f64 = 0.5;
/// The least stretch between two arrivals a delivery rate is read over: frames closer than
/// this reach a page bunched as often as the path delivered that fast.
pub const FLIGHT_RATE_SPAN_S: f64 = 0.05;
/// How near the pace the stream was written at, before the path showed late, the path
/// carried it to be neither narrowed under the stream (slower) nor draining a queue of the
/// stream's (faster): this share of it and its inverse.
pub const FLIGHT_KEEPS_UP: f64 = 0.9;
/// A page is not judged by a stamp this old.
pub const FLIGHT_DISTRUST_S: f64 = 2.0;
/// A page that stamps on and whose count passes no bulk frame for this long is not judged
/// by it: as long as the link gives a key frame to cross (`link::SEND_STALL_SECONDS`).
pub const FLIGHT_UNANSWERED_S: f64 = 10.0;
/// How long no round trip comes within the late line, those with no queue of the stream's
/// in them standing past it, before the least of those is the floor; how many of them at
/// the least, and how far apart at the most.
pub const FLIGHT_SETTLE_S: f64 = 3.0;
pub const FLIGHT_SETTLE_FRAMES: usize = 3;
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
/// after a stamp arrived, the next not being due, the path is taken to go on delivering at
/// the rate it showed.
pub const FLIGHT_STAMP_S: f64 = 0.05;
/// The stamps the way back's standing delay is read over.
const BACKS: usize = 4;
/// The bulk frames kept as out at most: past that the oldest are let go uncounted.
const OUT_MAX: usize = 1024;
/// The arrivals remembered at most, within the rate window.
const ARRIVALS_MAX: usize = 64;

/// The frame a binary message carries, as the page names it in its acks: a video frame's or
/// a JPEG stripe's id (`selkies-ws-core.ts` reads the same bytes on the message's arrival).
pub fn frame_id(data: &[u8]) -> Option<u16> {
    let named = match data.first()? {
        0x03 => data.len() > 6,
        0x04 => data.len() > 10,
        _ => false,
    };
    named.then(|| u16::from_be_bytes([data[2], data[3]]))
}

/// What a stamp says of the stream.
#[derive(Clone, Copy, Debug)]
pub struct Said {
    /// The bytes of the binary messages the page's socket has handed it.
    pub received: u64,
    /// The frame it last got, and how long before the stamp left, in seconds.
    pub frame: u16,
    pub held: f64,
}

/// A bulk frame written that the page has not counted.
#[derive(Debug)]
struct Sent {
    at: f64,
    /// Everything written through its end.
    through: u64,
    frame: Option<u16>,
    /// Whether it went out as the socket made room (behind a write that waited or just after
    /// one), when the stream itself would have written it.
    clocked: bool,
    /// Whether it found no bulk frame in flight: its round trip is the path's and its own
    /// crossing, with no queue of the stream's in it.
    alone: bool,
}

/// A frame's arrival: when it reached the page, on this clock, to the moment where the ack
/// named it (`exact`) and by when where not; everything written through its end; and when
/// it was written.
#[derive(Clone, Copy, Debug)]
struct Arrival {
    at: f64,
    through: u64,
    sent: f64,
    clocked: bool,
    exact: bool,
}

/// One socket's flight gauge.
#[derive(Debug, Default)]
pub struct Flight {
    out: VecDeque<Sent>,
    /// The bytes of the binary messages written, and of them what the page has counted.
    written: u64,
    delivered: u64,
    /// Everything written when the last bulk frame went out.
    bulk_written: u64,
    /// When the page's count last passed a bulk frame.
    answered_at: f64,
    /// When a bulk frame last asked to go, and the bound it asked under.
    asked_at: f64,
    bound: f64,
    /// The arrivals the page told of: of each stamp, the last frame its count passed.
    arrivals: VecDeque<Arrival>,
    /// Delivery rates the arrivals showed, in bytes a second, with when.
    rates: VecDeque<(f64, f64)>,
    floors: FloorHistory,
    floor: Option<f64>,
    trusted: bool,
    paced: bool,
    /// How far one round trip lies from the next on this path while nothing is paced,
    /// smoothed, and the last one.
    jitter: f64,
    last_round_trip: f64,
    /// When the path went late, when the stamp that showed it left, and the least round
    /// trip seen since.
    paced_at: f64,
    paced_mark: f64,
    paced_floor: f64,
    /// When the pacing last ended.
    unpaced_at: f64,
    /// The most the path delivered a second while it is paced, and while it last was: what
    /// it carries when it is given more than it takes, until it shows it carries more.
    paced_rate: f64,
    carried: Option<f64>,
    /// Since when no round trip came within the late line while those of frames that found
    /// none in flight stood past it, the least of those, how many, and when the last one
    /// came.
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
    /// What tells a way back held for longer than the lags' history from a clock: the least,
    /// over the same history, a named frame's arrival on the page's clock stood past its
    /// write on this one (the way out and the two clocks' distance, nothing of the way
    /// back in it), and the least round trip a named frame ever took on this clock alone,
    /// its ack's wait for the tick taken off (both ways at their shortest, no clock in it).
    /// The second less the first is where the lags' least stands with no hold in it.
    outs: FloorHistory,
    round: Option<f64>,
    /// What the way back held the last few stamps: the least of it stands on that way
    /// whatever one piece is held more than the next.
    backs: VecDeque<f64>,
}

impl Flight {
    /// The writer wrote a message at `now`, after a write that `waited` on the socket or
    /// went at once: `len` bytes of a binary one (none of any other, which the page does
    /// not count), a `bulk` frame or not, carrying `frame`.
    pub fn wrote(&mut self, len: usize, now: f64, bulk: bool, frame: Option<u16>, waited: bool) {
        self.written += len as u64;
        if waited {
            self.waited_at = now;
        }
        if !bulk {
            return;
        }
        // A path with nothing in flight since a while stood idle: a rate read across that
        // stretch would say nothing of what it carries.
        if self.out.is_empty() && now - self.answered_at > 2.0 * FLIGHT_STAMP_S {
            self.arrivals.clear();
        }
        let alone = self.trusted && self.delivered >= self.bulk_written;
        self.bulk_written = self.written;
        // Only a page that stamps has its frames told apart: one that does not is never
        // judged.
        if self.mark(now).is_none() {
            self.out.clear();
            return;
        }
        // A write that went at once into the room a wait just made is spaced by the path
        // as the wait was.
        let clocked = self.waited_at > 0.0 && now - self.waited_at < FLIGHT_RATE_SPAN_S;
        if self.out.len() >= OUT_MAX {
            self.out.pop_front();
        }
        self.out.push_back(Sent {
            at: now,
            through: self.written,
            frame,
            clocked,
            alone,
        });
    }

    /// A stamp that arrived at `now`: `stamp` is the page's own clock, in seconds, as it sent
    /// the ack, and `said` what the ack says of the stream as of then. How far behind this
    /// clock the stamp arrived, past the least any did, is how long the way back held it.
    /// Whether the page's count passed a bulk frame.
    pub fn note_stamp(&mut self, stamp: f64, said: Said, now: f64) -> bool {
        if !stamp.is_finite() || !said.held.is_finite() {
            return false;
        }
        // A count past what was written is another socket's, or of a message the writer has
        // yet to mark written, and one that went back is no count: neither says anything
        // of this stream, and the stamp that carries it is not taken, or its moment would
        // stand against frames its count was not read for.
        if said.received > self.written || said.received < self.delivered {
            return false;
        }
        let lag = now - stamp;
        // The least is read while the page's count moves, as the least round trip is: over
        // a still screen, where the page's acks go on and no frame does, neither history
        // moves, and the two forget a way back that has grown longer together.
        let least = if self.lags.is_empty() || now - self.answered_at <= FLIGHT_DISTRUST_S {
            self.lags.observe(lag, now)
        } else {
            self.lags.floor().unwrap_or(lag)
        };
        // An upload that holds the acks for longer than the history remembers leaves it no
        // lag from before: its least is then the hold's own, and would read the way back
        // as free. The way out says where the least stands without it, and is taken where
        // it is the lower of the two: a clock that drifted apart since moves both alike.
        let least = match (self.round, self.outs.floor()) {
            (Some(round), Some(out)) => least.min(round - out),
            _ => least,
        };
        self.back = (lag - least).max(0.0);
        self.stamped = true;
        self.stamped_at = now;
        self.backs.push_back(self.back);
        while self.backs.len() > BACKS {
            self.backs.pop_front();
        }
        // When the stamp left, on this clock.
        let mut left = now - self.back;
        self.delivered = said.received;
        // The last bulk frame the count passed, every one before it having arrived too.
        let mut passed = None;
        while self
            .out
            .front()
            .is_some_and(|sent| sent.through <= said.received)
        {
            passed = self.out.pop_front();
        }
        let Some(sent) = passed else {
            return false;
        };
        self.answered_at = now;
        // The ack names the frame the page last got and how long ago: where that is the
        // frame the count last passed, its arrival is known to the moment. Where it is not
        // (a page that names the frame it last presented, or padding behind the last
        // frame), it arrived by the time the stamp left.
        let named = sent.frame == Some(said.frame);
        let held = if named { said.held.max(0.0) } else { 0.0 };
        // No frame reaches a page before it is written. One that did, by more than two
        // clocks drift apart while the least is remembered, says the stamps' least is not
        // this page's clock any more: the clock stepped (a suspend the socket outlived), or
        // a stamp was nonsense. The history starts over at this stamp, where keeping it
        // would read every stamp as held, and the path beyond judging, for as long as it
        // remembers. A way back that holds the acks, however long, shows nothing of the
        // kind: what an ack says is as old as the ack.
        if left - held < sent.at - FLIGHT_LATE_S {
            self.outs = FloorHistory::default();
            self.round = None;
            self.lags = FloorHistory::default();
            self.lags.observe(lag, now);
            self.back = 0.0;
            self.backs.clear();
            self.backs.push_back(0.0);
            self.arrivals.clear();
            left = now;
        }
        if named {
            self.outs.observe(stamp - held - sent.at, now);
            let round = (now - sent.at - held).max(0.0);
            self.round = Some(self.round.map_or(round, |least| least.min(round)));
        }
        let arrived = (left - held).max(sent.at);
        let round_trip = arrived - sent.at;
        let floor = self.floors.observe(round_trip, now);
        self.floor = Some(floor);
        self.trusted = true;
        // A path's own unsteadiness is read while nothing is paced, and where a round trip
        // is shorter than the one before it: an uneven path's come down as often as they go
        // up, and a queue the stream builds only makes them longer, which is the lateness
        // itself and no reason to wait longer for it.
        if !self.paced && named {
            if self.last_round_trip > round_trip {
                let apart = self.last_round_trip - round_trip;
                self.jitter += (apart - self.jitter) * FLIGHT_JITTER_GAIN;
            }
            self.last_round_trip = round_trip;
        }
        // What arrived since a frame that arrived at least the span before, over the time
        // between the two.
        let before = self
            .arrivals
            .iter()
            .rev()
            .find(|arrival| arrived - arrival.at >= FLIGHT_RATE_SPAN_S)
            .copied();
        let rate = before
            .filter(|before| sent.through > before.through)
            .map(|before| (sent.through - before.through) as f64 / (arrived - before.at));
        if let Some(rate) = rate {
            self.rates.push_back((now, rate));
            if self.paced {
                self.paced_rate = self.paced_rate.max(rate);
            } else if self
                .carried
                .is_some_and(|carried| rate * FLIGHT_KEEPS_UP > carried)
            {
                self.carried = None;
            }
        }
        while self
            .rates
            .front()
            .is_some_and(|(at, _)| now - at > FLIGHT_RATE_WINDOW_S)
        {
            self.rates.pop_front();
        }
        if self.arrivals.back().is_none_or(|last| arrived > last.at) {
            self.arrivals.push_back(Arrival {
                at: arrived,
                through: sent.through,
                sent: sent.at,
                clocked: sent.clocked,
                exact: named,
            });
        }
        while self.arrivals.len() > ARRIVALS_MAX
            || (self.arrivals.len() > 1 && arrived - self.arrivals[1].at > FLIGHT_RATE_WINDOW_S)
        {
            self.arrivals.pop_front();
        }
        // A longer way round shows in every round trip, for as long as it lasts: in those
        // with no queue of the stream's in them, and in none coming within the line.
        let out = round_trip.max(floor);
        if out <= self.late_line() {
            self.over_since = None;
        } else if sent.alone {
            if self.over_since.is_none() || now - self.over_at > FLIGHT_SETTLE_GAP_S {
                self.over_since = Some(now);
                self.over_floor = out;
                self.over_count = 0;
            }
            self.over_floor = self.over_floor.min(out);
            self.over_count += 1;
            self.over_at = now;
            if self.over_count >= FLIGHT_SETTLE_FRAMES
                && self
                    .over_since
                    .is_some_and(|since| now - since >= FLIGHT_SETTLE_S)
            {
                self.move_floor(self.over_floor, now);
            }
        }
        if self.paced {
            self.paced_floor = self.paced_floor.min(out);
            // Two frames that arrived since the path showed late, written before it did,
            // while nothing was paced and by writes the socket took at once: did the bytes
            // between them cross at the pace they were written? The two lie far enough apart
            // that how far the path's round trips run apart is within the tenth a pace kept
            // may be off by, or an uneven path would show one kept as often as not. And they crossed at less
            // than the path carried when it was last paced: a stream at the rate of a
            // narrowed path, behind a queue of its own (a key frame), crosses at the pace it
            // is written too.
            let uneven = FLIGHT_JITTER_SLACK * self.jitter;
            let span = FLIGHT_RATE_SPAN_S.max(uneven / (1.0 - FLIGHT_KEEPS_UP));
            let before = self
                .arrivals
                .iter()
                .rev()
                .find(|arrival| arrived - arrival.at >= span)
                .copied();
            let kept_up = before.is_some_and(|before| {
                let (apart, sent_apart) = (arrived - before.at, sent.at - before.sent);
                let rate = sent.through.saturating_sub(before.through) as f64 / apart;
                named
                    && self
                        .carried
                        .is_none_or(|carried| rate < FLIGHT_KEEPS_UP * carried)
                    && before.exact
                    && before.at >= self.paced_mark
                    && before.sent >= self.unpaced_at
                    && sent.at <= self.paced_at
                    && !before.clocked
                    && !sent.clocked
                    && sent_apart > 0.0
                    && apart <= sent_apart / FLIGHT_KEEPS_UP
                    && apart >= sent_apart * FLIGHT_KEEPS_UP
            });
            // A path paced this long on end has shown its least round trip many times
            // over, whatever the floor says.
            let long = now - self.paced_at > FLIGHT_PACED_MAX_S;
            if (kept_up || long) && self.paced_floor.is_finite() {
                self.unpace(now);
                self.move_floor(self.paced_floor, now);
            }
        }
        true
    }

    /// When the page's last stamp left, on this clock: the latest moment at which a frame
    /// its count had not passed is known not to have reached it. `None` where no stamp
    /// says, none having come for `FLIGHT_DISTRUST_S`.
    fn mark(&self, now: f64) -> Option<f64> {
        (self.stamped && now - self.stamped_at <= FLIGHT_DISTRUST_S)
            .then_some(self.stamped_at - self.back)
    }

    /// How long its way back held the last stamp taken, past the least any took, while that
    /// stamp says anything: the ack it came with left the page that much before it arrived.
    pub fn way_back(&self, now: f64) -> f64 {
        if self.mark(now).is_some() {
            self.back
        } else {
            0.0
        }
    }

    /// How long the way back holds what the page sends, past the least it took: the least
    /// its last few stamps were held, while one says.
    fn held_back(&self, now: f64) -> f64 {
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
        // A path that is longer was not shown to carry less.
        self.carried = None;
    }

    /// How long on its way out past which the path is late: the floor, the bound, and the
    /// slack this path's own unsteadiness is given.
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
            // No stamp says what this path holds: it is not judged.
            if self.paced {
                self.unpace(now);
                self.rates.clear();
            }
            return true;
        };
        if self.delivered >= self.bulk_written {
            // No bulk frame is in flight: the pacing ends with the sender's backlog, or
            // with a stream that had stopped.
            let unpaced_at = self.unpaced_at;
            if self.paced && (idle || !more) {
                self.unpace(now);
                if idle {
                    // Nothing was paced over the pause: the stretch ended when it began.
                    self.unpaced_at = unpaced_at;
                }
                self.rates.clear();
            }
            return true;
        }
        let Some(oldest) = self.out.front().map(|sent| sent.at) else {
            // What is in flight was written before the page stamped: nothing tells of it.
            return true;
        };
        // The page stamps on, so a frame stands out behind what is still crossing, however
        // long: up to the span a key frame is given to cross.
        if now - oldest.max(self.answered_at) > FLIGHT_UNANSWERED_S {
            self.out.clear();
            self.rates.clear();
            self.arrivals.clear();
            self.over_since = None;
            self.trusted = false;
            self.unpace(now);
            return true;
        }
        // The oldest frame the page had not counted when its last stamp left has been on
        // its way out for at least that long.
        let late = mark - oldest > self.late_line();
        if late && !self.paced {
            self.paced = true;
            // The rates of the path as it was say nothing of what it carries now.
            self.rates.clear();
            self.paced_at = now;
            self.paced_mark = mark;
            self.paced_rate = 0.0;
            self.paced_floor = f64::INFINITY;
            self.last_round_trip = 0.0;
        }
        if !self.paced {
            return true;
        }
        let fits = self.fits(now, 0);
        if fits && !late && !more {
            self.unpace(now);
        }
        fits
    }

    /// Whether what is in flight on a paced path at `now`, and `more` bytes with it, is no
    /// more than the path holds on time. What is in flight includes a round trip's worth
    /// that has arrived and whose word is on its way back, and what arrived since the last
    /// stamp left; past that the bound may stand queued, or on a long path as much again as
    /// the round trip, without which the rate read off a window this small would never grow
    /// back to what the path carries.
    fn fits(&self, now: f64, more: usize) -> bool {
        let floor = self.floor.unwrap_or(0.0);
        let in_flight = (self.written - self.delivered) as f64 + more as f64;
        let since = (now - self.stamped_at).clamp(0.0, FLIGHT_STAMP_S);
        self.rate(now).is_some_and(|rate| {
            in_flight <= rate * (floor + self.bound.max(floor) + self.held_back(now) + since)
        })
    }

    /// Whether a paced path has room at `now` for a bulk frame of `len` bytes, asked by a
    /// sender that leaves out what there is no room for (`ws::relay`) and hands over the
    /// newest frame when there is, so none ages in the writer's hands: what `clear` would
    /// answer, with the frame itself counted in what must fit. A frame the writer already
    /// holds goes behind what fits, whatever its size; one that is yet to be handed over,
    /// a key frame as often as not where the encoder predicts no further back, goes when
    /// the path holds it on time too.
    pub fn would_clear(&self, now: f64, len: usize) -> bool {
        !self.trusted
            || !self.paced
            || self.mark(now).is_none()
            || self.delivered >= self.bulk_written
            || self.out.is_empty()
            || self.fits(now, len)
    }

    /// What a round trip may run past the floor and the bound on this path without being
    /// late: `FLIGHT_JITTER_SLACK` times how far its round trips lay apart while nothing was
    /// paced, a path that delivers unevenly being no later for it, less the half of the
    /// bound that unevenness has to itself. A path whose round trips stay within that half
    /// is late where an even one is.
    pub fn slack(&self) -> f64 {
        (FLIGHT_JITTER_SLACK * self.jitter - self.bound / 2.0).clamp(0.0, FLIGHT_SLACK_MAX_S)
    }

    fn unpace(&mut self, now: f64) {
        if self.paced {
            self.paced = false;
            self.unpaced_at = now;
            if self.paced_rate > 0.0 {
                self.carried = Some(self.paced_rate);
            }
        }
    }

    /// The most the path delivered a second over the rate window before `now`, in bytes,
    /// where a stamp said.
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

    /// What a paced path delivers a second at `now`, in bytes, once a stamp has said: all
    /// the socket's bytes, read off arrivals a stamp apart since the pacing began.
    pub fn paced_rate(&self, now: f64) -> Option<f64> {
        self.paced.then(|| self.rate(now)).flatten()
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

    /// The path's least round trip in seconds, once a stamp has said.
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
    /// queue of its own, and the page at its end: it counts the bytes of every message that
    /// reaches it, and its ack timer, once a first frame has, stamps an ack on each tick
    /// within a second of a frame's arrival and one every second after that, saying its
    /// count, the frame it last got and how long ago. Each way keeps the order of what it
    /// carries.
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
        /// On its way out: (arrival at the page, its bytes, the frame it carries).
        out: VecDeque<(f64, usize, Option<u16>)>,
        last_out: f64,
        /// On its way back: (arrival, the stamp, what the ack says).
        back: VecDeque<(f64, f64, Said)>,
        last_back: f64,
        /// The last frame written.
        frame: u16,
        /// The page's next tick, its last ack, what it counted, the frame it last got and
        /// when, and whether it stamps.
        tick: f64,
        acked: f64,
        counted: u64,
        got: Option<(u16, f64)>,
        stamping: bool,
        /// How much faster than this clock the page's runs, and how far it stands from it.
        drift: f64,
        clock: f64,
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
                frame: 0,
                tick: 0.0,
                acked: f64::NEG_INFINITY,
                counted: 0,
                got: None,
                stamping: true,
                drift: 0.0,
                clock: PAGE_CLOCK,
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
            let frame = bulk.then(|| {
                self.frame = self.frame.wrapping_add(1);
                self.frame
            });
            flight.wrote(len, now, bulk, frame, false);
            let at = (self.free_at + self.down + self.held()).max(self.last_out);
            self.last_out = at;
            self.out.push_back((at, len, frame));
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
                        let (at, len, frame) = self.out.pop_front().unwrap();
                        self.counted += len as u64;
                        if let Some(frame) = frame {
                            self.got = Some((frame, at));
                        }
                    }
                    (_, Some(tick)) => {
                        self.tick += PAGE_TICK;
                        let Some((frame, got)) = self.got else {
                            continue;
                        };
                        let due =
                            tick - got < PAGE_HEARTBEAT || tick - self.acked >= PAGE_HEARTBEAT;
                        if self.stamping && due {
                            self.acked = tick;
                            let stamp = tick * (1.0 + self.drift) + self.clock;
                            let said = Said {
                                received: self.counted,
                                frame,
                                held: tick - got,
                            };
                            let at = (tick + self.up + self.held()).max(self.last_back);
                            self.last_back = at;
                            self.back.push_back((at, stamp, said));
                        }
                    }
                    _ => break,
                }
            }
            while self.back.front().is_some_and(|(at, _, _)| *at <= now) {
                let (at, stamp, said) = self.back.pop_front().unwrap();
                flight.note_stamp(stamp, said, at);
            }
        }
    }

    /// A stamp the page sent at `now` that took no time back, having counted `received`
    /// bytes, the last of them frame `frame`'s, got `held` seconds before.
    fn stamp(flight: &mut Flight, now: f64, received: u64, frame: u16, held: f64) -> bool {
        let said = Said {
            received,
            frame,
            held,
        };
        flight.note_stamp(now + PAGE_CLOCK, said, now)
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
    /// `narrow` bytes a second from 2 s to 14 s), whose way back holds the page's acks
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
            if flight.paced() {
                paced += 0.001;
            }
            now += 0.001;
        }
        (longest, paced, path.worst)
    }

    /// Ways back that hold the page's acks up: an upload filling the uplink's
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
    fn what_holds_acks_up_on_their_way_back_holds_no_video() {
        // The acks arrive up to 1.2 s late, and each says when it left and what the page
        // had counted by then, which was all that had been written a round trip before:
        // nothing is paced and no frame waits.
        for (k, held) in WAYS_BACK.into_iter().enumerate() {
            let (longest, paced, _) = held_up_on_the_way_back(None, held);
            assert!(paced == 0.0 && longest < 0.002, "{k}: {paced} {longest}");
        }
    }

    #[test]
    fn a_narrowed_path_is_paced_whatever_holds_its_acks_up() {
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
        // The stream that starts again finds its acks that much later: its round trips'
        // least and its stamps' least were both read before, so the stamps still say when
        // they left, and nothing is paced.
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
        // 500 kB at 1.5 Mbit/s takes 2.7 s to cross, and 1.5 MB takes 8 s: the page counts
        // nothing meanwhile, and stamps its acks on. It is not a page whose count stopped
        // for good: the frames behind the key frame wait until it is counted, and nothing
        // goes at once.
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
    fn a_stamp_that_no_way_back_explains_starts_the_history_over() {
        // The stream runs on a path that carries it, one ack carries nonsense for a stamp,
        // and later the page's clock loses half a minute (a suspend the socket outlived).
        // Either would leave every later stamp reading as held for as long as the history
        // remembers, and the path beyond judging. The path then narrows: it is paced as a
        // page with a sound clock is.
        let mut flight = Flight::default();
        let mut path = Path::new(2_500_000.0, 0.02);
        let (mut waiting, mut due, mut now) = (0usize, 10.0, 10.0);
        let (mut paced_before, mut paced_after) = (0.0, 0.0);
        while now < 22.0 {
            if (12.0..12.001).contains(&now) {
                let said = Said {
                    received: path.counted,
                    frame: 0,
                    held: 0.0,
                };
                flight.note_stamp(1e300, said, now);
            }
            if (14.0..14.001).contains(&now) {
                path.clock -= 30.0;
            }
            if now >= 16.0 {
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
            if flight.paced() {
                if now < 16.0 {
                    paced_before += 0.001;
                } else {
                    paced_after += 0.001;
                }
            }
            now += 0.001;
        }
        assert!(paced_before == 0.0, "{paced_before}");
        assert!(
            paced_after > 5.0 && path.worst < 2.0,
            "{paced_after} {}",
            path.worst
        );
    }

    #[test]
    fn a_way_back_held_for_longer_than_a_page_goes_unjudged_is_still_a_way_back() {
        // An upload fills a buffer that holds twelve seconds of the page's uplink: every
        // ack arrives that much later, saying what the page had counted that much earlier,
        // which was all of the stream on a path that carries it. Nothing is paced, no frame
        // waits, and the stamps' history is kept: the hold is the way back's, not a clock's.
        let mut flight = Flight::default();
        let mut path = Path::new(2_500_000.0, 0.02);
        let (mut due, mut now) = (10.0, 10.0);
        let mut queue: VecDeque<f64> = VecDeque::new();
        let (mut longest, mut paced) = (0.0f64, 0.0);
        while now < 60.0 {
            let filled: f64 = (now - 15.0) / 10.0;
            path.up = 0.01 + filled.clamp(0.0, 1.0) * 12.0;
            path.deliver(&mut flight, now);
            if now >= due {
                queue.push_back(now);
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
        assert!((flight.back - 12.0).abs() < 0.1, "{}", flight.back);
    }

    #[test]
    fn a_stream_that_cannot_shrink_is_sent_at_the_rate_its_path_takes_each_frame_new() {
        // An encoder at its coarsest still makes 12.5 KiB of each frame, 60 a second, where
        // the steer asked for a fifth of that: a path narrowed to 187.5 kB a second takes
        // about fourteen of them. Their sender (`ws::relay`) hands the writer a frame only
        // as it would go at once and leaves out what comes meanwhile, telling the writer
        // that frames wait for as long as it does. The path is paced through the stretch and
        // never holds more than the bound and a frame, the frames that go are as many as it
        // takes, none waits, and once it is wide again all of them go, unpaced.
        const FRAME: usize = 12_800;
        const TOLD: f64 = 0.25;
        let mut flight = Flight::default();
        let mut path = Path::new(2_500_000.0, 0.02);
        let (mut due, mut now) = (10.0, 10.0);
        let mut left_out = f64::NEG_INFINITY;
        let (mut sent, mut gone, mut paced) = ([0u32; 3], [0u32; 3], [0.0f64; 3]);
        let mut worst = 0.0f64;
        while now < 22.0 {
            let phase = if now < 12.0 {
                0
            } else if now < 17.0 {
                1
            } else {
                2
            };
            path.rate = if phase == 1 { 187_500.0 } else { 2_500_000.0 };
            path.deliver(&mut flight, now);
            if now >= due {
                due += 1.0 / 60.0;
                let more = now - left_out <= TOLD;
                if flight.paced() && !flight.would_clear(now, FRAME) {
                    left_out = now;
                    gone[phase] += 1;
                } else if flight.clear(now, FLIGHT_LATE_S, more) {
                    path.write(&mut flight, FRAME, now, true);
                    sent[phase] += 1;
                } else {
                    // The frame the pacing began on is the one the writer holds: left out
                    // here, where the writer would write it once it is clear.
                    left_out = now;
                    gone[phase] += 1;
                }
                if now > 13.0 && now < 17.0 {
                    worst = worst.max(path.free_at - now);
                }
            }
            if flight.paced() {
                paced[phase] += 0.001;
            }
            now += 0.001;
        }
        assert!(gone[0] == 0 && paced[0] == 0.0, "{gone:?} {paced:?}");
        assert!(
            paced[1] > 4.5,
            "paced through the narrow stretch: {paced:?}"
        );
        assert!(
            (60..=75).contains(&sent[1]),
            "about 14 frames a second of the 60: {sent:?}"
        );
        assert!(
            worst < FLIGHT_LATE_S + 1.5 * FRAME as f64 / 187_500.0,
            "the path holds the bound and a frame at the most: {worst}"
        );
        assert!(
            paced[2] < 0.5 && gone[2] < 20 && sent[2] > 280,
            "and wide again it takes them all: {paced:?} {gone:?} {sent:?}"
        );
    }

    #[test]
    fn a_way_back_held_past_the_stamps_history_is_a_way_back_still() {
        // The lags' least is the least of the history's ten minutes. An upload that holds
        // the acks for longer outlives it: every lag it remembers is then a held one. The
        // way out is not held, and with the least round trip the path ever showed it says
        // where the least stands without the hold: behind a megabyte of buffer or a
        // relay's queue alike, the hold is read to the millisecond thirteen minutes on,
        // nothing is paced as the history turns or after, and no frame waits.
        for hold in [1.3f64, 0.143] {
            let mut flight = Flight::default();
            let mut path = Path::new(2_500_000.0, 0.02);
            let (mut due, mut now) = (10.0, 10.0);
            let mut queue: VecDeque<f64> = VecDeque::new();
            let (mut paced, mut longest) = (0.0f64, 0.0f64);
            while now < 790.0 {
                let filled: f64 = now - 15.0;
                path.up = 0.01 + filled.clamp(0.0, 1.0) * hold;
                path.deliver(&mut flight, now);
                if now >= due {
                    queue.push_back(now);
                    due += 1.0 / 60.0;
                }
                while !queue.is_empty() && flight.clear(now, FLIGHT_LATE_S, queue.len() > 1) {
                    path.write(&mut flight, 16_384, now, true);
                    longest = longest.max(now - queue.pop_front().unwrap());
                }
                if flight.paced() {
                    paced += 0.001;
                }
                if now > 20.0 {
                    assert!(
                        (flight.back - hold).abs() < 0.01,
                        "{hold} at {now}: {}",
                        flight.back
                    );
                }
                now += 0.001;
            }
            assert!(paced == 0.0 && longest < 0.002, "{hold}: {paced} {longest}");
        }
    }

    #[test]
    fn a_page_clock_that_loses_time_for_half_an_hour_hides_no_more_than_the_history_holds() {
        // A page whose clock loses 100 parts in a million against this one, on a path that
        // carries the stream: every lag is longer than the last, the least of the history
        // is ten minutes old, and each ack reads as held by what the clock lost since, 60
        // ms and no more however long it goes on. The path then narrows: it is paced, and
        // holds that much more than the bound at the most.
        let mut flight = Flight::default();
        let mut path = Path::new(2_500_000.0, 0.02);
        path.drift = -100e-6;
        let (mut waiting, mut due, mut now) = (0usize, 10.0, 10.0);
        let (mut most, mut paced, mut worst) = (0.0f64, 0.0f64, 0.0f64);
        while now < 1815.0 {
            if now >= 1810.0 {
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
            if now < 1810.0 {
                most = most.max(flight.back);
                assert!(!flight.paced(), "paced at {now}");
            } else {
                if flight.paced() {
                    paced += 0.001;
                }
                if now > 1811.0 {
                    worst = worst.max(path.free_at - now);
                }
            }
            now += 0.001;
        }
        assert!((0.05..0.07).contains(&most), "{most}");
        assert!(
            paced > 4.0 && worst < FLIGHT_LATE_S + 0.2,
            "{paced} {worst}"
        );
    }

    #[test]
    fn a_page_that_stamps_nothing_is_not_judged() {
        // Its frames stand out as a narrowed path's do, and nothing says so: the stream is
        // written as it comes.
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

    /// How long after a path narrows to 1.5 Mbit/s under 8 Mbit/s of 16 KiB frames the pacing
    /// begins, and what the path holds then, in seconds of its new rate: with a round trip
    /// of `round_trip` and up to `jitter` more each way.
    fn onset(round_trip: f64, jitter: f64, seed: u64) -> (f64, f64) {
        let mut flight = Flight::default();
        let mut path = Path::new(2_500_000.0, round_trip);
        path.jitter = jitter;
        path.draw = seed * 7919;
        let (mut waiting, mut due, mut now) = (0usize, 10.0, 10.0);
        while now < 25.0 {
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
            if flight.paced() && now >= 20.0 {
                return (now - 20.0, path.free_at - now);
            }
            now += 0.001;
        }
        (f64::INFINITY, 0.0)
    }

    #[test]
    fn a_queue_the_stream_builds_is_not_read_as_the_paths_unevenness() {
        // The round trips of the frames that still arrive over the narrowed path grow one
        // after the other, which says nothing of how unevenly the path delivers: the line
        // stays where it stood, and the pacing begins with the first stamp past it, within
        // a fifth of a second and with under a second of the new rate in the path. Reading
        // every distance between two round trips would have the line run from the queue
        // for a tenth of a second more, and the path hold half as much again.
        for round_trip in [0.002, 0.02] {
            let (at, holds) = onset(round_trip, 0.0, 1);
            assert!(at < 0.2 && holds < 0.8, "{round_trip}: {at} {holds}");
        }
        // A path uneven within half the bound has its line where an even one does: what
        // its frames arrive later by is its acks' own way back.
        for seed in 1..=8 {
            let (at, holds) = onset(0.04, 0.04, seed);
            assert!(at < 0.22 && holds < 0.95, "{seed}: {at} {holds}");
        }
    }

    #[test]
    fn a_path_uneven_within_half_the_bound_is_late_where_an_even_one_is() {
        // Up to 40 ms more each way, piece by piece, under a stream the path carries: its
        // round trips never run half the bound past the least, nothing is paced in twenty
        // seconds, and the line stands where an even path's does.
        for seed in 1..=8u64 {
            let mut path = Path::new(2_500_000.0, 0.04);
            path.jitter = 0.04;
            path.draw = seed * 7919;
            let run = stream(true, &mut path, 16_384, 1.0 / 60.0, 20.0, None, 0.0);
            assert!(
                run.paced == 0.0 && run.waiting == 0,
                "{seed}: {}",
                run.paced
            );
        }
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
        // by both, and the path carries what it did: the frames stand out past the bound,
        // their arrivals then show the stream crossing as it was sent, and nothing waits
        // for long.
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

    /// The path narrows to 1.5 Mbit/s under 8 Mbit/s at 12 s and is paced; the frames that
    /// waited are let go (the gate), and at 14 s the stream comes back cut to what the path
    /// carries, a key frame of 80 kB first: the path's least round trip at the end, and the
    /// most a byte waited in its queue from 15 s on. `jitter` is how unevenly it delivers.
    fn behind_its_own_key_frame(jitter: f64, seed: u64) -> (f64, f64) {
        let mut flight = Flight::default();
        let mut path = Path::new(2_500_000.0, 0.02);
        path.jitter = jitter;
        path.draw = seed * 7919;
        let (mut waiting, mut due, mut now) = (0usize, 10.0, 10.0);
        let mut key = true;
        while now < 20.0 {
            if now >= 12.0 {
                path.rate = 187_500.0;
            }
            if (13.5..14.0).contains(&now) {
                waiting = 0;
                due = 14.0;
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
                let len = match (now < 14.0, key) {
                    (true, _) => 16_384,
                    (false, true) => 80_000,
                    (false, false) => 3_125,
                };
                key = key && now < 14.0;
                path.write(&mut flight, len, now, true);
                waiting -= 1;
            }
            now += 0.001;
        }
        (flight.floor().unwrap(), path.worst)
    }

    #[test]
    fn a_stream_at_the_narrowed_paths_rate_behind_its_own_key_frame_is_no_longer_way_round() {
        // The frames behind the key frame reach the page as far apart as they were written,
        // each the path's whole rate for its sixtieth of a second: the pace is kept, behind
        // a queue that is the stream's own.
        let (floor, worst) = behind_its_own_key_frame(0.0, 1);
        assert!(floor < 0.05 && worst < 0.3, "{floor} {worst}");
        // On a path that delivers up to 40 ms unevenly each way two frames a twentieth of a
        // second apart show a pace kept as often as not, whatever the path did.
        for seed in 1..=8 {
            let (floor, worst) = behind_its_own_key_frame(0.04, seed);
            assert!(floor < 0.1 && worst < 0.35, "{seed}: {floor} {worst}");
        }
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
    fn a_count_tells_of_every_frame_it_passed() {
        let mut flight = Flight::default();
        stamp(&mut flight, 10.0, 0, 0, 0.0);
        flight.wrote(1000, 10.0, true, Some(1), false);
        flight.wrote(1000, 10.03, true, Some(2), false);
        flight.wrote(1000, 10.06, true, Some(3), false);
        assert!(stamp(&mut flight, 10.08, 3000, 3, 0.01));
        assert_eq!(flight.in_flight(), 0);
        // One that passes no further frame tells nothing new, and one past what was written
        // is not this socket's.
        assert!(!stamp(&mut flight, 10.1, 3000, 3, 0.03));
        assert!(!stamp(&mut flight, 10.12, 9000, 3, 0.05));
        assert_eq!(flight.in_flight(), 0);
        // A frame the count has not passed stands out, and goes on holding the next until
        // a count passes it.
        flight.wrote(16_384, 10.2, true, Some(4), false);
        stamp(&mut flight, 10.35, 3000, 3, 0.28);
        assert!(!flight.clear(10.35, FLIGHT_LATE_S, true));
        assert!(stamp(&mut flight, 10.36, 3000 + 16_384, 4, 0.0));
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
    fn a_stamp_whose_count_is_not_taken_says_nothing_of_the_frames_out() {
        let mut flight = Flight::default();
        stamp(&mut flight, 10.0, 0, 0, 0.0);
        flight.wrote(1000, 10.0, true, Some(1), false);
        assert!(stamp(&mut flight, 10.02, 1000, 1, 0.0));
        flight.wrote(16_384, 10.05, true, Some(2), false);
        // The page counted a message the writer has handed the socket and not yet marked
        // written: the ack is ahead of this gauge, and its moment is not held against the
        // frame out, which its count would have passed.
        assert!(!stamp(&mut flight, 10.3, 1000 + 16_384 + 500, 2, 0.2));
        assert!(flight.clear(10.3, FLIGHT_LATE_S, true));
        assert!(!flight.paced());
        flight.wrote(500, 10.3, false, None, false);
        assert!(stamp(&mut flight, 10.35, 1000 + 16_384 + 500, 2, 0.25));
        assert!(flight.clear(10.35, FLIGHT_LATE_S, true));
        assert!(!flight.paced());
    }

    #[test]
    fn a_held_frame_goes_once_no_bulk_frame_is_in_flight() {
        // A path of 150 ms with sparse video: a frame stands out past the line, and the
        // next one finds the path late and waits. It goes as soon as a stamp says the frame
        // before it arrived, with no rate ever read.
        let mut flight = Flight::default();
        stamp(&mut flight, 10.0, 0, 0, 0.0);
        flight.wrote(1000, 10.0, true, Some(1), false);
        assert!(stamp(&mut flight, 10.15, 1000, 1, 0.0));
        flight.wrote(16_384, 11.0, true, Some(2), false);
        stamp(&mut flight, 11.26, 1000, 1, 1.11);
        assert!(!flight.clear(11.26, FLIGHT_LATE_S, false));
        assert!(stamp(&mut flight, 11.30, 1000 + 16_384, 2, 0.0));
        assert!(flight.clear(11.30, FLIGHT_LATE_S, false));
        assert!(!flight.paced());
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
    fn a_longer_way_round_too_few_frames_could_show_becomes_the_floor_in_time() {
        // A frame every 0.4 s and a round trip that steps from 20 ms to 500 ms, half of it
        // each way: one frame is out at a time, too few for two to show the pace
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
    fn frames_written_as_the_socket_pushed_back_say_nothing_of_the_pace_kept() {
        // The same arrivals as a longer way round gives, of frames that went out behind
        // writes the socket made wait: they were spaced by what the path took, and vouch
        // for nothing.
        for (clocked, kept) in [(false, true), (true, false)] {
            let mut flight = Flight::default();
            stamp(&mut flight, 10.0, 0, 0, 0.0);
            flight.wrote(1000, 10.0, true, Some(1), false);
            assert!(stamp(&mut flight, 10.02, 1000, 1, 0.0));
            for k in 0..6u16 {
                let at = 10.1 + f64::from(k) * 0.03;
                flight.wrote(16_384, at, true, Some(2 + k), clocked);
            }
            stamp(&mut flight, 10.3, 1000, 1, 0.28);
            assert!(!flight.clear(10.3, FLIGHT_LATE_S, true));
            assert!(flight.paced());
            for k in 0..6u16 {
                let at = 10.32 + f64::from(k) * 0.03;
                let counted = 1000 + 16_384 * (u64::from(k) + 1);
                assert!(stamp(&mut flight, at, counted, 2 + k, 0.0));
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
        // and the uneven arrivals' rates allowing that much, where the path left to itself
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
        // the bound: every frame's round trip stands past the line, but the pacing keeps a
        // frame in flight, so none is written into an empty path to vouch for a longer way
        // round: the floor stays, and so does the pacing.
        let mut flight = Flight::default();
        let mut path = Path::new(2_500_000.0, 0.02);
        let (mut waiting, mut due, mut now) = (0usize, 10.0, 10.0);
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
            now += 0.001;
        }
        assert!(flight.floor().unwrap() < 0.05, "{:?}", flight.floor());
        assert!(flight.paced());
        assert!(path.worst < 0.3, "{}", path.worst);
    }

    #[test]
    fn a_key_frame_holds_the_frames_behind_it_only_until_it_arrived() {
        // 600 KiB at once over 20 Mbit/s crosses in a quarter of a second, past the bound:
        // the frames behind it wait for the stamp that counts it, which also says the rate
        // it crossed at.
        let mut flight = Flight::default();
        let mut path = Path::new(2_500_000.0, 0.02);
        // The page stamps once a first frame has reached it, and its frames are told apart
        // from then on.
        path.write(&mut flight, 1000, 9.8, true);
        path.deliver(&mut flight, 9.95);
        path.write(&mut flight, 1000, 9.95, true);
        path.deliver(&mut flight, 10.0);
        path.write(&mut flight, 1000, 10.0, true);
        path.deliver(&mut flight, 10.05);
        path.write(&mut flight, 614_400, 10.05, true);
        assert!(flight.clear(10.06, FLIGHT_LATE_S, true));
        path.write(&mut flight, 16_384, 10.06, true);
        path.deliver(&mut flight, 10.12);
        assert!(flight.clear(10.12, FLIGHT_LATE_S, true), "not late yet");
        path.deliver(&mut flight, 10.22);
        assert!(
            !flight.clear(10.22, FLIGHT_LATE_S, true),
            "late, no rate yet"
        );
        path.deliver(&mut flight, 10.37);
        assert!(flight.clear(10.37, FLIGHT_LATE_S, true));
        assert!(flight.rate(10.37).unwrap() > 1_000_000.0);
    }

    #[test]
    fn audio_counts_in_flight_and_only_bulk_frames_stand_out() {
        let mut flight = Flight::default();
        // Written before the page stamped anything: nothing tells of it.
        flight.wrote(16_384, 9.9, true, Some(1), false);
        assert!(!stamp(&mut flight, 9.95, 16_384, 1, 0.0));
        flight.wrote(16_384, 9.95, true, Some(2), false);
        assert!(stamp(&mut flight, 9.96, 2 * 16_384, 2, 0.0));
        flight.wrote(500, 10.0, false, None, false);
        flight.wrote(16_384, 10.0, true, Some(3), false);
        flight.wrote(16_384, 10.01, true, Some(4), false);
        assert_eq!(flight.in_flight(), 500 + 2 * 16_384);
        assert!(stamp(&mut flight, 10.02, 3 * 16_384 + 500, 3, 0.0));
        assert_eq!(flight.in_flight(), 16_384);
        // Audio the page counts passes no frame.
        flight.wrote(500, 10.03, false, None, false);
        assert!(!stamp(&mut flight, 10.04, 3 * 16_384 + 1000, 3, 0.02));
        assert_eq!(flight.in_flight(), 16_384);
        // A message the page does not count (a text, a ping) is none of the gauge's.
        flight.wrote(0, 10.05, false, None, false);
        assert_eq!(flight.in_flight(), 16_384);
    }

    #[test]
    fn a_page_whose_count_never_moved_is_not_judged() {
        let mut flight = Flight::default();
        for k in 0..100u16 {
            let now = 10.0 + f64::from(k) * 0.05;
            stamp(&mut flight, now, 0, 0, 0.0);
            flight.wrote(16_384, now, true, Some(k), false);
            assert!(flight.clear(now, FLIGHT_LATE_S, true));
        }
        // One whose count moved and then stopped, the page stamping on, is held to it for
        // as long as a key frame may take to cross, and left alone after.
        let mut flight = Flight::default();
        stamp(&mut flight, 10.0, 0, 0, 0.0);
        flight.wrote(1000, 10.0, true, Some(1), false);
        assert!(stamp(&mut flight, 10.01, 1000, 1, 0.0));
        flight.wrote(16_384, 11.0, true, Some(2), false);
        for k in 0..18 {
            let now = 11.5 + f64::from(k) * 0.5;
            stamp(&mut flight, now, 1000, 1, now - 10.01);
            assert!(!flight.clear(now, FLIGHT_LATE_S, true), "{now}");
            assert!(flight.paced());
        }
        let now = 11.0 + FLIGHT_UNANSWERED_S + 0.1;
        stamp(&mut flight, now, 1000, 1, now - 10.01);
        assert!(flight.clear(now, FLIGHT_LATE_S, true));
        assert!(!flight.paced());
    }
}
