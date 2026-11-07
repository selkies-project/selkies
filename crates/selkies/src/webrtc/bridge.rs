/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! What stands between a display's capture and its relay: a queue that keeps the wire decodable
//! across a drop and holds the stream to its steered rate.
//!
//! `maxsize` selects the buffering policy: depth 1 is latest-wins (video wants the freshest
//! frame), a deeper bound a short drop-oldest FIFO (audio wants continuity, so a brief consumer
//! stall does not silently drop samples).
//!
//! A drop happens upstream of RTP: no sequence number is spent, so the receiver sees no gap and
//! never asks for a key frame, while every delta frame behind the drop references a picture it
//! never received. With `request_keyframe` bound the bridge keeps the wire decodable the way the
//! WebSockets relay does. A frame that names what it predicts from is dropped with a word to the
//! encoder (`invalidate_reference`), which then predicts past it, and only the frames predicting
//! from a dropped one are held back, so the stream resumes on the next frame without a key frame.
//! The ids let go are forgotten once a frame the encoder began after the latest word goes out:
//! frames reach the bridge in encode order, so nothing coded before the word can follow it, while
//! an id recurs every 65536 frames, which an infinite GOP outlasts. A second frame held back that
//! way although its encode began after the word went out says the encoder never heard it (a word
//! sent while the capture is still starting is lost) and will keep predicting from what the bridge
//! let go, so a key frame is asked for, as for a closed gate, until one arrives. The first such
//! frame proves nothing, since the encoder reads its words just before it stamps an encode, and the
//! frames it coded before the word are held without one however far the consumer lags. A frame
//! without an encode instant counts instead, past `LOST_CHAIN_FRAMES` in a row. A frame that names
//! nothing closes a gate that holds delta frames back, a key frame is asked for until one arrives
//! and reopens it, and a queued key frame is never evicted by a delta frame. A gate no key frame
//! answers within `GATE_TIMEOUT_S` reopens on its own.
//!
//! A bridge given its display's steered rate (`set_budget`) also holds the stream to it. An encoder
//! meets a rate by coarser quantizers, down to the coarsest it has, and content the coarsest
//! cannot fit (noise, film grain) overshoots, which a path sized for the rate queues and loses,
//! and whatever repairs the loss adds more. The frames let through fill a bucket that drains at
//! the rate, and while it holds more than `BUDGET_WINDOW_S` of the rate a delta frame that names
//! what it predicts from is dropped with the word to the encoder, so the frame rate falls instead
//! of the latency rising, by no more than `BUDGET_RUN_MAX` frames in a row from an encoder that has
//! sent no anchor: one predicting from its last frames only codes a key frame past a longer run,
//! and the key frame overshoots the rate again, so the stream is thinned, not cut. Key frames pass
//! uncharged: charged, a large one would drop the delta
//! frames behind it for as long as it takes on a slow path. Neither is a frame `FRAME_NUM_WRAP`
//! frames or a multiple of it past a key frame dropped: an H.264 decoder that misses the frame
//! whose `frame_num` wraps to 0 cannot be predicted past the gap, so the encoder answers such a
//! drop with a key frame. Queued, such an H.264 frame is not let go for a newer frame either: the
//! newer one is, as behind a key frame.
//!
//! Every call runs under the bridge's own lock on the capture's fan-out task; only the consumer's
//! wait is asynchronous.

use std::collections::VecDeque;
use std::sync::Arc;

use log::warn;
use parking_lot::Mutex;
use selkies_rtc::rtp::EncodedFrame;
use selkies_rtc::rtp::clock;
use selkies_rtc::rtp::codecs::Codec;
use tokio::sync::Notify;

use crate::display::capture::{FRAME_NUM_WRAP, LOST_FRAME_MEMORY};

/// Floor between key-frame requests for one display: a request inside it is served by the key
/// frame the previous one already scheduled.
pub const IDR_REQUEST_FLOOR_S: f64 = 0.25;
/// A closed gate no key frame answers within this long reopens on its own, so an encoder that
/// ignores requests degrades to a smear rather than a dead stream.
pub const GATE_TIMEOUT_S: f64 = 1.0;
/// Frames without an encode instant a bridge holds back, predicting from one it let go, before it
/// takes the encoder for one that never predicted past the drop.
pub const LOST_CHAIN_FRAMES: u32 = 3;
/// How far past its steered rate a bridge lets frames run, in seconds of that rate.
pub const BUDGET_WINDOW_S: f64 = 0.25;
/// The most delta frames in a row the budget drops from a stream whose encoder keeps no long-term
/// anchor (`BridgeItem::anchor`): such an encoder predicts from its last frames only, the dropped
/// ones among them, and a longer run leaves it nothing to predict from but a key frame, which
/// overshoots the rate again.
pub const BUDGET_RUN_MAX: u32 = 6;

/// What the bridge reads of a frame.
pub trait BridgeItem: Send {
    /// The frame's id and what it predicts from, where the encoder tracks them.
    fn dependency(&self) -> Option<(u16, Option<u16>)> {
        None
    }
    /// When its encode began, on the bridge's clock in seconds.
    fn encode_start_s(&self) -> Option<f64> {
        None
    }
    /// Whether the encoder marked the frame a long-term anchor.
    fn anchor(&self) -> bool {
        false
    }
    /// Whether the frame is H.264, whose `frame_num` can wrap.
    fn is_h264(&self) -> bool {
        false
    }
    /// Its bytes.
    fn byte_len(&self) -> usize {
        0
    }
}

impl BridgeItem for Arc<EncodedFrame> {
    fn dependency(&self) -> Option<(u16, Option<u16>)> {
        self.dependency
    }

    fn encode_start_s(&self) -> Option<f64> {
        self.timing
            .filter(|t| t.encode_start_ns > 0)
            .map(|t| t.encode_start_ns as f64 / 1e9)
    }

    fn anchor(&self) -> bool {
        self.anchor
    }

    fn is_h264(&self) -> bool {
        self.codec == Some(Codec::H264)
    }

    fn byte_len(&self) -> usize {
        self.data.len()
    }
}

/// A clock in seconds.
pub type BridgeClock = Box<dyn Fn() -> f64 + Send>;

/// The hooks a video bridge calls, under its lock.
pub struct BridgeHooks {
    /// Asks the display's encoder for a key frame, at most once per `IDR_REQUEST_FLOOR_S`.
    pub request_keyframe: Box<dyn FnMut() + Send>,
    /// Tells the encoder a frame was dropped; `None` gates every drop behind a key frame.
    pub invalidate_reference: Option<Box<dyn FnMut(u16) + Send>>,
}

struct State<T> {
    queue: VecDeque<T>,
    maxsize: usize,
    hooks: Option<BridgeHooks>,
    clock: BridgeClock,
    queued_keyframe: bool,
    gated_at: Option<f64>,
    last_request: Option<f64>,
    lost: VecDeque<u16>,
    told_at: Option<f64>,
    held: u32,
    dropped: u64,
    invalidated: u64,
    budget_bps: Option<f64>,
    budget_bytes: f64,
    budget_at: f64,
    over_budget: u64,
    since_key: u32,
    /// Budget drops in a row, and whether the encoder has sent an anchor (`BUDGET_RUN_MAX`).
    budget_run: u32,
    anchored: bool,
    /// Delta frames naming their reference let through, beside `over_budget`.
    passed: u64,
}

/// One queue between a capture and its consumer (module docblock).
pub struct PipelineBridge<T> {
    state: Mutex<State<T>>,
    available: Notify,
}

impl<T: BridgeItem> PipelineBridge<T> {
    /// A bridge of `maxsize` items, gated when `hooks` are bound.
    pub fn new(maxsize: usize, hooks: Option<BridgeHooks>, clock_fn: Option<BridgeClock>) -> Self {
        Self {
            state: Mutex::new(State {
                queue: VecDeque::with_capacity(maxsize),
                maxsize,
                hooks,
                clock: clock_fn.unwrap_or_else(|| Box::new(clock::monotonic)),
                queued_keyframe: false,
                gated_at: None,
                last_request: None,
                lost: VecDeque::with_capacity(LOST_FRAME_MEMORY),
                told_at: None,
                held: 0,
                dropped: 0,
                invalidated: 0,
                budget_bps: None,
                budget_bytes: 0.0,
                budget_at: 0.0,
                over_budget: 0,
                since_key: 0,
                budget_run: 0,
                anchored: false,
                passed: 0,
            }),
            available: Notify::new(),
        }
    }

    /// Hold the frames let through to `bps`, the display's steered rate; `None` lets every
    /// frame through.
    pub fn set_budget(&self, bps: Option<f64>) {
        let mut s = self.state.lock();
        match bps.filter(|b| *b > 0.0) {
            None => {
                s.budget_bps = None;
                s.budget_bytes = 0.0;
            }
            Some(bps) => {
                if s.budget_bps.is_none() {
                    s.budget_at = (s.clock)();
                }
                s.budget_bps = Some(bps);
            }
        }
    }

    /// Items discarded since construction, evicted for a newer one or held back behind a closed
    /// gate. None of them spent a sequence number, so they appear in no loss statistic on either
    /// side; this counter is what separates a lagging sender from a lossy link.
    pub fn dropped(&self) -> u64 {
        self.state.lock().dropped
    }

    /// The share of `dropped` the encoder was told to predict past.
    pub fn invalidated(&self) -> u64 {
        self.state.lock().invalidated
    }

    /// Recent use of the frame budget, as a share of its buffered window. The same accounting
    /// that admits frames drives this reading, so it cannot mistake a mere frame for a filled
    /// allowance.
    pub fn budget_fill(&self) -> Option<f64> {
        let mut s = self.state.lock();
        let bps = s.budget_bps?;
        Some(s.budget_level(bps) / (bps / 8.0 * BUDGET_WINDOW_S))
    }

    /// Frames dropped for the steered rate.
    pub fn over_budget(&self) -> u64 {
        self.state.lock().over_budget
    }

    /// Delta frames naming their reference let through, beside `over_budget`.
    pub fn passed(&self) -> u64 {
        self.state.lock().passed
    }

    /// Whether nothing is queued.
    pub fn is_empty(&self) -> bool {
        self.state.lock().queue.is_empty()
    }

    /// Enqueue an item, dropping the oldest when the queue is full; `keyframe` says whether it
    /// decodes on its own (module docblock).
    pub fn set_data(&self, data: T, keyframe: bool) {
        let mut s = self.state.lock();
        s.set_data(data, keyframe);
        let queued = !s.queue.is_empty();
        drop(s);
        if queued {
            self.available.notify_one();
        }
    }

    /// The oldest item now, if any.
    pub fn try_take(&self) -> Option<T> {
        self.state.lock().queue.pop_front()
    }

    /// Wait until an item is available and return it.
    pub async fn get_data(&self) -> T {
        loop {
            let notified = self.available.notified();
            if let Some(item) = self.try_take() {
                return item;
            }
            notified.await;
        }
    }
}

impl<T: BridgeItem> State<T> {
    fn set_data(&mut self, data: T, keyframe: bool) {
        if self.hooks.is_none() {
            if self.queue.len() >= self.maxsize {
                self.queue.pop_front();
                self.dropped += 1;
            }
            self.queue.push_back(data);
            return;
        }
        if keyframe {
            if self.queue.len() >= self.maxsize {
                self.queue.pop_front();
                self.dropped += 1;
            }
            self.queue.push_back(data);
            self.queued_keyframe = true;
            self.gated_at = None;
            self.lost.clear();
            self.held = 0;
            self.since_key = 0;
            self.budget_run = 0;
            return;
        }
        self.since_key += 1;
        let can_invalidate = self
            .hooks
            .as_ref()
            .is_some_and(|h| h.invalidate_reference.is_some());
        if can_invalidate && let Some((frame_id, reference)) = data.dependency() {
            self.set_tracked(data, frame_id, reference);
            return;
        }
        let now = (self.clock)();
        if let Some(gated_at) = self.gated_at {
            if now - gated_at < GATE_TIMEOUT_S {
                self.dropped += 1;
                self.ask(now);
                return;
            }
            warn!(target: "webrtc", "Video bridge: no key frame within {GATE_TIMEOUT_S:.1}s of a drop; sending delta frames again");
            self.gated_at = None;
        }
        if self.queue.len() >= self.maxsize {
            self.dropped += 1;
            if !self.queued_keyframe {
                self.queue.pop_front();
                self.dropped += 1;
            }
            self.gated_at = Some(now);
            self.ask(now);
            return;
        }
        self.queue.push_back(data);
        self.queued_keyframe = false;
    }

    /// A delta frame that names what it predicts from.
    fn set_tracked(&mut self, data: T, frame_id: u16, reference: Option<u16>) {
        let timing = data.encode_start_s();
        if reference.is_some_and(|r| self.lost.contains(&r)) {
            self.hold(frame_id, timing);
            return;
        }
        let at_wrap = self.since_key.is_multiple_of(FRAME_NUM_WRAP);
        let kept = data.is_h264() && at_wrap;
        self.anchored |= data.anchor();
        if !at_wrap && self.budget_full() && (self.anchored || self.budget_run < BUDGET_RUN_MAX) {
            self.over_budget += 1;
            self.budget_run += 1;
            self.drop_item(frame_id);
            return;
        }
        self.budget_run = 0;
        if self.queue.len() >= self.maxsize {
            if self.queued_keyframe {
                self.drop_item(frame_id);
                return;
            }
            if let Some(evicted) = self.queue.pop_front() {
                match evicted.dependency() {
                    Some((id, _)) => self.drop_item(id),
                    None => self.dropped += 1,
                }
            }
            if reference.is_some_and(|r| self.lost.contains(&r)) {
                self.hold(frame_id, timing);
                return;
            }
        }
        let bytes = data.byte_len();
        self.queue.push_back(data);
        self.charge(bytes);
        self.passed += 1;
        self.queued_keyframe = kept;
        self.held = 0;
        if !self.lost.is_empty()
            && let (Some(told), Some(encoded)) = (self.told_at, timing)
            && encoded > told
        {
            self.lost.clear();
        }
    }

    fn budget_level(&mut self, bps: f64) -> f64 {
        let now = (self.clock)();
        self.budget_bytes = (self.budget_bytes - (now - self.budget_at) * bps / 8.0).max(0.0);
        self.budget_at = now;
        self.budget_bytes
    }

    fn budget_full(&mut self) -> bool {
        match self.budget_bps {
            Some(bps) => self.budget_level(bps) > bps / 8.0 * BUDGET_WINDOW_S,
            None => false,
        }
    }

    fn charge(&mut self, bytes: usize) {
        if let Some(bps) = self.budget_bps {
            self.budget_bytes = self.budget_level(bps) + bytes as f64;
        }
    }

    fn ask(&mut self, now: f64) {
        if self
            .last_request
            .is_some_and(|last| now - last < IDR_REQUEST_FLOOR_S)
        {
            return;
        }
        self.last_request = Some(now);
        if let Some(hooks) = self.hooks.as_mut() {
            (hooks.request_keyframe)();
        }
    }

    fn remember_lost(&mut self, frame_id: u16) {
        if self.lost.len() == LOST_FRAME_MEMORY {
            self.lost.pop_front();
        }
        self.lost.push_back(frame_id);
    }

    /// Let a frame go and tell the encoder, so nothing later predicts from it.
    fn drop_item(&mut self, frame_id: u16) {
        self.dropped += 1;
        self.invalidated += 1;
        self.held = 0;
        self.told_at = Some((self.clock)());
        self.remember_lost(frame_id);
        if let Some(invalidate) = self
            .hooks
            .as_mut()
            .and_then(|h| h.invalidate_reference.as_mut())
        {
            invalidate(frame_id);
        }
    }

    /// Hold back a frame predicting from one already let go, and ask for a key frame when the
    /// encoder coded it after it was told of the drop.
    fn hold(&mut self, frame_id: u16, encoded: Option<f64>) {
        self.dropped += 1;
        self.remember_lost(frame_id);
        let unheard = match (encoded, self.told_at) {
            (None, _) => {
                self.held += 1;
                self.held > LOST_CHAIN_FRAMES
            }
            (Some(encoded), Some(told)) if encoded > told => {
                self.held += 1;
                self.held > 1
            }
            _ => false,
        };
        if unheard {
            let now = (self.clock)();
            self.ask(now);
        }
    }
}

#[cfg(test)]
mod tests {
    //! The video bridge's gate, on stand-in frames and a manual clock.

    use std::sync::Mutex as StdMutex;

    use super::*;

    #[derive(Clone, Debug, PartialEq)]
    struct F {
        name: String,
        dependency: Option<(u16, Option<u16>)>,
        at: Option<f64>,
        size: usize,
        h264: bool,
        anchor: bool,
    }

    impl BridgeItem for F {
        fn dependency(&self) -> Option<(u16, Option<u16>)> {
            self.dependency
        }
        fn encode_start_s(&self) -> Option<f64> {
            self.at
        }
        fn anchor(&self) -> bool {
            self.anchor
        }
        fn is_h264(&self) -> bool {
            self.h264
        }
        fn byte_len(&self) -> usize {
            self.size
        }
    }

    fn plain(name: &str) -> F {
        F {
            name: name.into(),
            dependency: None,
            at: None,
            size: 0,
            h264: false,
            anchor: false,
        }
    }

    fn tracked(id: u32, reference: Option<u32>) -> F {
        F {
            name: format!("F{id}"),
            dependency: Some(((id & 0xFFFF) as u16, reference.map(|r| (r & 0xFFFF) as u16))),
            at: None,
            size: 0,
            h264: false,
            anchor: false,
        }
    }

    fn timed(id: u32, reference: Option<u32>, at: f64) -> F {
        F {
            at: Some(at),
            ..tracked(id, reference)
        }
    }

    #[derive(Clone, Default)]
    struct Rig {
        now: Arc<StdMutex<f64>>,
        requests: Arc<StdMutex<Vec<f64>>>,
        forgotten: Arc<StdMutex<Vec<u16>>>,
    }

    impl Rig {
        fn new() -> Rig {
            let rig = Rig::default();
            *rig.now.lock().unwrap() = 1000.0;
            rig
        }
        fn at(&self) -> f64 {
            *self.now.lock().unwrap()
        }
        fn step(&self, s: f64) {
            *self.now.lock().unwrap() += s;
        }
        fn requests(&self) -> Vec<f64> {
            self.requests.lock().unwrap().clone()
        }
        fn forgotten(&self) -> Vec<u16> {
            self.forgotten.lock().unwrap().clone()
        }
        fn bridge(&self, invalidates: bool) -> PipelineBridge<F> {
            let (requests, now) = (self.requests.clone(), self.now.clone());
            let forgotten = self.forgotten.clone();
            let clock_now = self.now.clone();
            PipelineBridge::new(
                1,
                Some(BridgeHooks {
                    request_keyframe: Box::new(move || {
                        requests.lock().unwrap().push(*now.lock().unwrap());
                    }),
                    invalidate_reference: invalidates.then(|| {
                        Box::new(move |id| forgotten.lock().unwrap().push(id))
                            as Box<dyn FnMut(u16) + Send>
                    }),
                }),
                Some(Box::new(move || *clock_now.lock().unwrap())),
            )
        }
    }

    fn drain(bridge: &PipelineBridge<F>) -> Vec<String> {
        let mut out = Vec::new();
        while let Some(item) = bridge.try_take() {
            out.push(item.name);
        }
        out
    }

    #[test]
    fn scenario() {
        let rig = Rig::new();
        let bridge = rig.bridge(false);
        bridge.set_data(plain("P0"), false);
        assert_eq!(
            drain(&bridge),
            ["P0"],
            "a delta frame queues while the sender keeps up"
        );
        assert_eq!(bridge.dropped(), 0);
        assert!(rig.requests().is_empty());

        bridge.set_data(plain("P1"), false);
        bridge.set_data(plain("P2"), false);
        assert!(bridge.is_empty(), "a drop discards both delta frames");
        assert_eq!(bridge.dropped(), 2);
        assert_eq!(rig.requests(), [rig.at()], "and asks for a key frame");
        rig.step(0.05);
        bridge.set_data(plain("P3"), false);
        assert!(
            bridge.is_empty(),
            "delta frames behind the gap are held back"
        );
        assert_eq!(bridge.dropped(), 3);
        assert_eq!(
            rig.requests().len(),
            1,
            "a request inside the floor is not repeated"
        );

        rig.step(0.05);
        bridge.set_data(plain("IDR1"), true);
        rig.step(0.02);
        bridge.set_data(plain("P4"), false);
        assert_eq!(
            drain(&bridge),
            ["IDR1"],
            "a delta frame never evicts a queued key frame"
        );
        assert_eq!(bridge.dropped(), 4);
        rig.step(0.02);
        bridge.set_data(plain("P5"), false);
        assert!(
            bridge.is_empty(),
            "the chain behind that key frame is broken again"
        );
        assert_eq!(bridge.dropped(), 5);

        rig.step(IDR_REQUEST_FLOOR_S);
        bridge.set_data(plain("P6"), false);
        assert_eq!(
            rig.requests().len(),
            2,
            "a held delta frame past the floor asks again"
        );
        assert_eq!(*rig.requests().last().unwrap(), rig.at());

        bridge.set_data(plain("IDR2"), true);
        let mut got = drain(&bridge);
        bridge.set_data(plain("P7"), false);
        got.extend(drain(&bridge));
        bridge.set_data(plain("P8"), false);
        got.extend(drain(&bridge));
        assert_eq!(
            got,
            ["IDR2", "P7", "P8"],
            "delta frames flow behind a delivered key frame"
        );

        let before = rig.requests().len();
        bridge.set_data(plain("IDR4"), true);
        bridge.set_data(plain("IDR5"), true);
        assert_eq!(
            drain(&bridge),
            ["IDR5"],
            "a newer key frame replaces a queued one"
        );
        assert_eq!(rig.requests().len(), before, "without closing the gate");
        bridge.set_data(plain("P10"), false);
        assert_eq!(drain(&bridge), ["P10"]);

        bridge.set_data(plain("P11"), false);
        bridge.set_data(plain("P12"), false);
        rig.step(GATE_TIMEOUT_S + 0.01);
        bridge.set_data(plain("P13"), false);
        assert_eq!(
            drain(&bridge),
            ["P13"],
            "a gate no key frame answers lets delta frames through"
        );

        let audio: PipelineBridge<F> = PipelineBridge::new(3, None, None);
        for i in 0..5 {
            audio.set_data(plain(&format!("A{i}")), true);
        }
        assert_eq!(
            drain(&audio),
            ["A2", "A3", "A4"],
            "a deeper bridge drops oldest, keeps order"
        );
        assert_eq!(audio.dropped(), 2);
    }

    #[test]
    fn references() {
        let rig = Rig::new();
        let bridge = rig.bridge(true);
        bridge.set_data(tracked(0, None), true);
        let mut got = drain(&bridge);
        bridge.set_data(tracked(1, Some(0)), false);
        got.extend(drain(&bridge));
        assert_eq!(got, ["F0", "F1"]);
        assert!(rig.forgotten().is_empty());

        bridge.set_data(tracked(2, Some(1)), false);
        bridge.set_data(tracked(3, Some(2)), false);
        assert!(bridge.is_empty(), "a drop holds what predicts from it");
        assert_eq!(
            rig.forgotten(),
            [2],
            "and tells the encoder which frame went"
        );
        assert_eq!(bridge.dropped(), 2);
        assert!(rig.requests().is_empty());
        bridge.set_data(tracked(4, Some(3)), false);
        assert!(
            bridge.is_empty(),
            "a frame predicting from a held frame is held too"
        );
        assert_eq!(bridge.dropped(), 3);
        assert_eq!(rig.forgotten(), [2]);
        bridge.set_data(tracked(5, Some(1)), false);
        assert_eq!(
            drain(&bridge),
            ["F5"],
            "the frame predicting past the drop flows"
        );
        assert!(rig.requests().is_empty());

        bridge.set_data(tracked(6, None), true);
        bridge.set_data(tracked(7, Some(6)), false);
        assert_eq!(
            drain(&bridge),
            ["F6"],
            "a frame behind a queued key frame is dropped and named"
        );
        assert_eq!(rig.forgotten(), [2, 7]);
        bridge.set_data(tracked(8, Some(6)), false);
        assert_eq!(drain(&bridge), ["F8"]);
        bridge.set_data(tracked(9, Some(8)), false);
        bridge.set_data(tracked(10, Some(8)), false);
        assert_eq!(
            drain(&bridge),
            ["F10"],
            "an eviction does not hold back an unrelated newer frame"
        );
        assert_eq!(rig.forgotten(), [2, 7, 9]);
    }

    #[test]
    fn unheard() {
        let rig = Rig::new();
        let bridge = rig.bridge(true);
        bridge.set_data(timed(0, None, rig.at()), true);
        bridge.set_data(timed(1, Some(0), rig.at()), false);
        let told = rig.at();
        assert_eq!(drain(&bridge), ["F0"]);
        assert_eq!(rig.forgotten(), [1]);
        let mut fid = 2;
        for _ in 0..LOST_CHAIN_FRAMES + 3 {
            rig.step(0.016);
            bridge.set_data(timed(fid, Some(fid - 1), told - 0.001), false);
            fid += 1;
        }
        assert!(bridge.is_empty(), "frames coded before the word are held");
        assert!(rig.requests().is_empty(), "without a key frame");
        bridge.set_data(timed(fid, Some(fid - 1), told + 0.004), false);
        fid += 1;
        assert!(
            rig.requests().is_empty(),
            "the first coded after the word proves nothing"
        );
        rig.step(0.016);
        bridge.set_data(timed(fid, Some(fid - 1), told + 0.02), false);
        fid += 1;
        assert_eq!(rig.requests().len(), 1, "a second one asks for a key frame");
        for _ in 0..3 {
            rig.step(0.016);
            bridge.set_data(timed(fid, Some(fid - 1), rig.at()), false);
            fid += 1;
        }
        assert_eq!(rig.requests().len(), 1, "not again inside the floor");
        rig.step(IDR_REQUEST_FLOOR_S);
        bridge.set_data(timed(fid, Some(fid - 1), rig.at()), false);
        fid += 1;
        assert_eq!(rig.requests().len(), 2, "again past it");
        bridge.set_data(timed(fid, None, rig.at()), true);
        let mut got = drain(&bridge);
        bridge.set_data(timed(fid + 1, Some(fid), rig.at()), false);
        got.extend(drain(&bridge));
        assert_eq!(got, [format!("F{fid}"), format!("F{}", fid + 1)]);
        assert_eq!(rig.forgotten(), [1]);
        fid += 1;
        for _ in 0..LOST_CHAIN_FRAMES + 2 {
            rig.step(IDR_REQUEST_FLOOR_S);
            bridge.set_data(timed(fid + 1, Some(fid), rig.at()), false);
            drain(&bridge);
            fid += 1;
        }
        assert_eq!(rig.requests().len(), 2, "frames that flow ask for nothing");

        let rig = Rig::new();
        let bridge = rig.bridge(true);
        bridge.set_data(tracked(0, None), true);
        bridge.set_data(tracked(1, Some(0)), false);
        drain(&bridge);
        for fid in 2..2 + LOST_CHAIN_FRAMES {
            bridge.set_data(tracked(fid, Some(fid - 1)), false);
        }
        assert!(rig.requests().is_empty(), "held up to the run");
        bridge.set_data(
            tracked(2 + LOST_CHAIN_FRAMES, Some(1 + LOST_CHAIN_FRAMES)),
            false,
        );
        assert_eq!(rig.requests().len(), 1, "one more asks for a key frame");
    }

    #[test]
    fn wrap() {
        let rig = Rig::new();
        let bridge = rig.bridge(true);
        bridge.set_data(timed(0, None, rig.at()), true);
        drain(&bridge);
        for fid in 1..5 {
            rig.step(0.016);
            bridge.set_data(timed(fid, Some(fid - 1), rig.at()), false);
            drain(&bridge);
        }
        rig.step(0.016);
        bridge.set_data(timed(5, Some(4), rig.at()), false);
        rig.step(0.016);
        bridge.set_data(timed(6, Some(5), rig.at() - 0.001), false);
        let told = rig.at();
        bridge.set_data(timed(7, Some(6), told - 0.0005), false);
        assert!(bridge.is_empty());
        assert_eq!(rig.forgotten(), [5]);
        assert!(rig.requests().is_empty());
        rig.step(0.016);
        bridge.set_data(timed(8, Some(4), rig.at()), false);
        assert_eq!(drain(&bridge), ["F8"]);
        assert!(rig.requests().is_empty());
        let end = 0x10000 + 8;
        let mut fid = 9;
        while fid < end {
            rig.step(0.016);
            bridge.set_data(timed(fid, Some(fid - 1), rig.at()), false);
            if bridge.is_empty() {
                break;
            }
            drain(&bridge);
            fid += 1;
        }
        assert_eq!(
            fid, end,
            "the recurring ids of the frames let go flow like any other"
        );
        assert_eq!(bridge.dropped(), 3);
        assert_eq!(rig.forgotten(), [5]);
        assert!(rig.requests().is_empty());
    }

    #[test]
    fn budget_runs() {
        let rig = Rig::new();
        let rate = 80_000.0;
        let max_gap = |bridge: &PipelineBridge<F>, first: u32, reference: u32, anchor: bool| {
            let (mut passed, mut last) = (Vec::new(), reference);
            for fid in first..first + 180 {
                rig.step(1.0 / 60.0);
                let item = F {
                    size: 5000,
                    anchor,
                    ..timed(fid, Some(last), rig.at())
                };
                bridge.set_data(item, false);
                if bridge.try_take().is_some() {
                    passed.push(fid);
                    last = fid;
                }
            }
            passed
                .windows(2)
                .map(|w| w[1] - w[0] - 1)
                .max()
                .unwrap_or(0)
        };
        let bridge = rig.bridge(true);
        bridge.set_budget(Some(rate));
        bridge.set_data(
            F {
                size: 100,
                ..tracked(0, None)
            },
            true,
        );
        drain(&bridge);
        assert_eq!(
            max_gap(&bridge, 1, 0, false),
            BUDGET_RUN_MAX,
            "a stream thirty times over the rate, from an encoder that sent no anchor, is thinned: \
             no more than BUDGET_RUN_MAX frames dropped in a row"
        );
        let anchored = rig.bridge(true);
        anchored.set_budget(Some(rate));
        anchored.set_data(
            F {
                size: 100,
                ..tracked(1000, None)
            },
            true,
        );
        drain(&anchored);
        assert!(
            max_gap(&anchored, 1001, 1000, true) > BUDGET_RUN_MAX,
            "from one that keeps anchors, which predicts past any run, the budget drops longer runs"
        );
    }

    #[test]
    fn budget_fill_is_used_capacity_not_frame_presence() {
        let rig = Rig::new();
        let bridge = rig.bridge(true);
        assert_eq!(bridge.budget_fill(), None);
        bridge.set_budget(Some(80_000.0));
        assert_eq!(bridge.budget_fill(), Some(0.0));

        bridge.set_data(tracked(0, None), true);
        assert_eq!(drain(&bridge), ["F0"]);
        bridge.set_data(
            F {
                size: 4000,
                ..tracked(1, Some(0))
            },
            false,
        );
        assert_eq!(drain(&bridge), ["F1"]);
        let fill = bridge.budget_fill().unwrap();
        assert!(fill > 1.5 && fill < 1.7, "one filling frame: {fill}");
        rig.step(0.5);
        assert_eq!(bridge.budget_fill(), Some(0.0), "idle time refunds it");
        bridge.set_budget(None);
        assert_eq!(bridge.budget_fill(), None);
    }

    #[test]
    fn budget() {
        let rig = Rig::new();
        let rate = 80_000.0;
        let window = rate / 8.0 * BUDGET_WINDOW_S;
        let bridge = rig.bridge(true);
        let stream = |first: u32, size: usize, seconds: f64, reference: u32| {
            let (mut passed, mut sent, mut last, mut fid) = (Vec::new(), 0usize, reference, first);
            for _ in 0..(seconds * 60.0) as u32 {
                rig.step(1.0 / 60.0);
                let item = F {
                    size,
                    ..timed(fid, Some(last), rig.at())
                };
                bridge.set_data(item, false);
                if bridge.try_take().is_some() {
                    passed.push(fid);
                    sent += size;
                    last = fid;
                }
                fid += 1;
            }
            (sent, passed, fid, last)
        };
        bridge.set_budget(Some(rate));
        bridge.set_data(
            F {
                size: 100,
                ..tracked(0, None)
            },
            true,
        );
        drain(&bridge);
        let (_, passed, fid, last) = stream(1, 150, 2.0, 0);
        assert_eq!(passed.len(), 120, "a stream within the rate passes whole");
        assert!(rig.forgotten().is_empty());
        let (sent, passed, fid, last) = stream(fid, 500, 10.0, last);
        let want = rate / 8.0 * 10.0;
        assert!(
            want <= sent as f64 && sent as f64 <= want + window + 500.0,
            "a stream three times over the rate is held to it: {sent} {want}"
        );
        assert!(rig.requests().is_empty());
        assert_eq!(bridge.over_budget(), 600 - passed.len() as u64);
        assert_eq!(rig.forgotten().len() as u64, bridge.over_budget());
        for f in (121..721u32).filter(|f| f % FRAME_NUM_WRAP == 0) {
            assert!(
                passed.contains(&f),
                "a frame where frame_num can wrap is never dropped: {f}"
            );
            assert!(!rig.forgotten().contains(&(f as u16)));
        }
        rig.step(5.0);
        bridge.set_data(
            F {
                size: 8000,
                ..tracked(fid, None)
            },
            true,
        );
        assert_eq!(
            drain(&bridge),
            [format!("F{fid}")],
            "a key frame past the window passes"
        );
        let key = fid;
        let dropped = rig.forgotten().len();
        for n in 1..=2 {
            rig.step(1.0 / 60.0);
            bridge.set_data(
                F {
                    size: 100,
                    ..tracked(key + n, Some(key + n - 1))
                },
                false,
            );
            assert_eq!(
                drain(&bridge).len(),
                1,
                "it is not charged, so the frames behind it pass"
            );
        }
        assert_eq!(rig.forgotten().len(), dropped);
        bridge.set_budget(None);
        let (_, passed, ..) = stream(key + 3, 500, 2.0, key + 2);
        assert_eq!(
            passed.len(),
            120,
            "a lifted budget lets every frame through"
        );
        let _ = last;

        let plain_bridge = rig.bridge(false);
        plain_bridge.set_budget(Some(rate));
        plain_bridge.set_data(
            F {
                size: 100,
                ..plain("K")
            },
            true,
        );
        drain(&plain_bridge);
        let mut delivered = 0;
        for n in 0..120 {
            rig.step(1.0 / 60.0);
            plain_bridge.set_data(
                F {
                    size: 500,
                    ..plain(&format!("P{n}"))
                },
                false,
            );
            delivered += drain(&plain_bridge).len();
        }
        assert_eq!(
            delivered, 120,
            "frames that name nothing are never dropped for the rate"
        );
        assert_eq!(plain_bridge.over_budget(), 0);
    }

    #[test]
    fn kept() {
        let rig = Rig::new();
        let bridge = rig.bridge(true);
        let h264 = |id, reference| F {
            h264: true,
            anchor: false,
            ..tracked(id, reference)
        };
        bridge.set_data(h264(0, None), true);
        drain(&bridge);
        bridge.set_data(h264(1, Some(0)), false);
        bridge.set_data(h264(2, Some(0)), false);
        assert_eq!(
            drain(&bridge),
            ["F2"],
            "the frame after a key frame is let go for a newer one"
        );
        assert_eq!(rig.forgotten(), [1]);
        for n in 3..FRAME_NUM_WRAP {
            bridge.set_data(h264(n, Some(n - 1)), false);
            drain(&bridge);
        }
        bridge.set_data(h264(FRAME_NUM_WRAP, Some(FRAME_NUM_WRAP - 1)), false);
        bridge.set_data(h264(FRAME_NUM_WRAP + 1, Some(FRAME_NUM_WRAP)), false);
        assert_eq!(
            drain(&bridge),
            [format!("F{FRAME_NUM_WRAP}")],
            "it stays queued, the newer let go"
        );
        assert_eq!(rig.forgotten(), [1, (FRAME_NUM_WRAP + 1) as u16]);

        let vp8 = rig.bridge(true);
        vp8.set_data(tracked(0, None), true);
        drain(&vp8);
        vp8.set_data(tracked(1, Some(0)), false);
        vp8.set_data(tracked(2, Some(0)), false);
        assert_eq!(
            drain(&vp8),
            ["F2"],
            "a codec without frame_num keeps the newest frame"
        );
        assert_eq!(*rig.forgotten().last().unwrap(), 1);
    }
}
