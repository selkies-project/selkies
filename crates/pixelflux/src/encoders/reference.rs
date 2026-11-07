//! Which frame a delivered frame predicts from, kept true across the frames a client reports
//! lost.

use std::collections::VecDeque;

/// How many frames back a prediction may reach: the decoded picture buffer every session asks
/// for, where its level admits it.
pub const REFERENCE_FRAMES: u32 = 8;

/// The most long-term frames a window keeps as anchors, out of the decoded picture buffer, so a
/// loss older than every recent frame still finds one before it.
pub const ANCHORS: usize = 2;

/// Two anchors take a frame every `ANCHOR_EVERY` frames after a key frame by turns, the lost or
/// older one first, so one is always twelve to twenty-three frames old.
const ANCHOR_EVERY: u64 = 12;

/// One anchor takes a frame every `ANCHOR_ALONE_EVERY` frames after a key frame: a loss just
/// before it is reported once it has moved on, which a longer period makes rarer.
const ANCHOR_ALONE_EVERY: u64 = 48;

/// An anchor predicts from the newest frame every consumer was sent (`settle`) only within an
/// anchor period and `SETTLE_SLACK` frames of it, a consumer that took the last anchor. One
/// further behind, for which every anchor would predict from far back and cost every consumer
/// the bits, is predicted past on its own report instead (from the pinned anchor, where it comes
/// to that). The frame where H.264's `frame_num` wraps reaches back however far: a decoder that
/// misses it cannot be predicted past.
const SETTLE_SLACK: u64 = 4;

/// Of two anchors, the one marked predicts from the frame every consumer was sent (`settle`) only
/// where that is one of the `SETTLE_RECENT` newest recent frames, which the buffer keeps beside two
/// anchors on every device (`nvenc::anchor_count`). Further back the anchor costs the moving
/// picture: NVENC's HEVC on a 1080p page scrolling at 8 Mbit/s, every consumer's acknowledgement
/// late, averaged 49.3 dB from two frames back, 49.0 from three, 38.5 from four and 27.4 from the
/// other anchor a period back (the frame sent to all of them past the recent ones), against 49.7.
/// The anchor predicts from the frame before it then, and a consumer that loses frames is predicted
/// past from the pinned anchor on its own report.
const SETTLE_RECENT: usize = 2;

/// The schedule keeps an anchor every consumer was sent until they all hold it, for up to
/// `PENDING_PERIODS` anchor periods: a consumer slower to report than a period, as one on a
/// narrow link is, would otherwise never see an anchor pinned past the key frame.
const PENDING_PERIODS: u64 = 4;

/// A lone anchor every consumer holds is not marked over while the newest frame they were all
/// sent runs more than `HOLD_LAG` frames ahead of the newest they all hold, as when a consumer's
/// link goes dark: a consumer reporting its loss once it is back is predicted past only from an
/// anchor it holds. Past `HOLD_MAX` frames it is marked over all the same, a consumer that far
/// behind taking a key frame.
const HOLD_LAG: u64 = ANCHOR_ALONE_EVERY / 2;
const HOLD_MAX: u64 = 8 * ANCHOR_ALONE_EVERY;

/// What the consumers say of a frame, applied in the order they said it ahead of an encode.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReferenceReport {
    /// A consumer lost the frame (`invalidate_reference`).
    Lost(u16),
    /// Every consumer holds the frame (`ReferenceWindow::acknowledge`).
    Held(u16),
    /// Every consumer was sent the frame, which one may yet lose (`ReferenceWindow::acknowledge`).
    Sent(u16),
    /// The consumers' sender let the frame go unsent to hold the stream to a rate, as it was handed
    /// over (`invalidate_reference(withheld=True)`): lost to every consumer.
    Withheld(u16),
}

/// The frame a delivered frame predicts from.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Reference {
    /// A session that cannot say which frame it predicted from, or leave a lost one out.
    #[default]
    Untracked,
    /// A frame that decodes on its own.
    None,
    /// The id of the frame predicted from.
    Frame(u16),
}

impl Reference {
    /// The wire form: the frame id, -1 for a frame that decodes on its own, -2 where the
    /// session does not track its references.
    pub fn frame_id(self) -> i32 {
        match self {
            Reference::Untracked => -2,
            Reference::None => -1,
            Reference::Frame(id) => id as i32,
        }
    }
}

/// What an invalidation asks of the encoder.
#[derive(Debug, PartialEq, Eq)]
pub enum Invalidation {
    /// The frame predates the last key frame, was already removed, or was never a reference.
    Ignored,
    /// Forget the frame carrying this timestamp and every frame after it.
    Forget(u64),
    /// The frame has left the window, so every reference still held predicts through it: the
    /// next frame has to be a key frame.
    KeyFrame,
}

/// The reference frames a session's decoder holds, in encode order, with the ones a client
/// reported lost marked.
///
/// An encoder told to forget a frame leaves it and every frame after it out of its predictions,
/// predicts the next frame from the newest frame before them, and codes a key frame when none is
/// left. The window mirrors that, so the reference a frame carries on the wire is the one its
/// bitstream uses, and it asks for the key frame where the encoder would. Frames are addressed
/// by the capture's frame id, which wraps; the timestamp the encoder is told to forget is the
/// session's own count of encoded frames, which does not.
///
/// An H.264 session also names how many values its `frame_num` takes before wrapping
/// (`set_frame_num_range`). A decoder that never receives the frame carrying `frame_num` 0 sees
/// a gap across that wrap, and FFmpeg's H.264 decoder, which the browsers decode with on Linux,
/// derives the picture order past such a gap a wrap short and drops the pictures after it until
/// that order passes the last one shown (13 of a range of 16 after two frames lost), so a
/// loss covering that frame is answered with a key frame rather than a prediction past it.
///
/// A window `with_anchors` also keeps long-term frames out of the buffer: the key frame is the
/// first anchor and frames after it are marked on a schedule, and a loss older than every recent
/// frame is predicted past from the newest anchor before it.
///
/// Where the consumers say which frames every one of them holds, or was sent (`acknowledge`),
/// several of them share the stream and each recovers on its own: the newest anchor all of them
/// hold is pinned, the schedule marking the other, so a loss of any depth is predicted past from a
/// frame every consumer has; an anchor predicts from a frame they were all sent where that is near
/// (`settle`), so each can decode it; and in H.264 the frame where `frame_num` wraps is an anchor
/// too.
pub struct ReferenceWindow {
    frames: VecDeque<(u16, u64, bool)>,
    capacity: usize,
    lends: bool,
    anchors: Vec<Option<(u16, u64, bool)>>,
    recent: VecDeque<(u16, u64, bool)>,
    next_pts: u64,
    age: KeyAge,
    key_pts: u64,
    frame_num_range: u64,
    acknowledged: bool,
    common: VecDeque<u64>,
    sent: VecDeque<u64>,
}

/// How far a frame id lies behind the newest frame recorded, in capture frames counted across
/// the id's wrap, and whether it went out at or after the last key frame. A session can run far
/// longer than the id takes to wrap between key frames, so the ids alone cannot order a frame
/// against the key.
#[derive(Default)]
struct KeyAge {
    newest: u16,
    since_key: u64,
}

impl KeyAge {
    fn record(&mut self, frame_id: u16, key: bool) {
        self.since_key = if key {
            0
        } else {
            self.since_key + frame_id.wrapping_sub(self.newest) as u64
        };
        self.newest = frame_id;
    }

    /// How many frames `frame_id` went out before the newest one; `None` for a frame before the
    /// last key frame or one never sent.
    fn behind(&self, frame_id: u16) -> Option<u64> {
        let behind = self.newest.wrapping_sub(frame_id) as u64;
        (behind < 0x8000 && behind <= self.since_key).then_some(behind)
    }
}

impl ReferenceWindow {
    pub fn new(capacity: u32) -> Self {
        Self {
            frames: VecDeque::new(),
            capacity: capacity.max(1) as usize,
            lends: false,
            anchors: Vec::new(),
            recent: VecDeque::new(),
            next_pts: 0,
            age: KeyAge::default(),
            key_pts: 0,
            frame_num_range: 0,
            acknowledged: false,
            common: VecDeque::new(),
            sent: VecDeque::new(),
        }
    }

    /// A window over a decoded picture buffer of `capacity` frames, `count` of them anchors (one
    /// or two).
    pub fn with_anchors(capacity: u32, count: usize) -> Self {
        let mut w = Self::new(capacity);
        w.anchors = vec![None; count.clamp(1, ANCHORS)];
        w.set_capacity(capacity);
        w
    }

    /// An anchor holding no frame, or a lost one, lends its share of the buffer to the recent
    /// frames, as NVENC's AV1 encoder does with a long-term slot it holds nothing in.
    pub fn lend_free_anchors(&mut self) {
        self.lends = true;
    }

    /// The recent frames the buffer holds: its share beside the anchors, and that of each anchor
    /// lending its own (`lend_free_anchors`).
    fn recent_capacity(&self) -> usize {
        let free = self
            .anchors
            .iter()
            .filter(|a| a.is_none_or(|a| a.2))
            .count();
        self.capacity + if self.lends { free } else { 0 }
    }

    /// Whether the window keeps anchors.
    pub fn anchored(&self) -> bool {
        !self.anchors.is_empty()
    }

    /// How many anchors the window keeps.
    pub fn anchor_count(&self) -> usize {
        self.anchors.len()
    }

    /// The anchor the next frame is marked into, if any: the first for a key frame or while none
    /// is held, and on the schedule after it the one that is empty, lost, or older, never the one
    /// pinned (`acknowledge`) where there are two, nor one every consumer was sent and is yet to
    /// hold (`PENDING_PERIODS`), nor a lone one they all hold while frames they were all sent go
    /// unheld (`HOLD_LAG`) or while it would predict from the one it marks over (`reaches_back`).
    /// Acknowledged, an H.264 frame where `frame_num` wraps is an anchor too.
    pub fn plan_anchor(&self, key: bool) -> Option<u8> {
        if self.anchors.is_empty() {
            return None;
        }
        if key || !self.has_reference() || self.anchors.iter().all(Option::is_none) {
            return Some(0);
        }
        let wraps = self.wraps_next();
        if !(self.next_pts - self.key_pts).is_multiple_of(self.anchor_period()) && !wraps {
            return None;
        }
        if self.anchors.len() == 1 && self.acknowledged && !wraps && self.reaches_back() {
            return None;
        }
        if self.anchors.len() == 1
            && self.acknowledged
            && !wraps
            && let Some((_, pts, false)) = self.anchors[0]
            && self.is_common(pts)
        {
            let held = self
                .common
                .back()
                .map_or(self.key_pts, |&c| c.max(self.key_pts));
            let sent = self.sent.back().map_or(held, |&s| s.max(held));
            if sent - held > HOLD_LAG && self.next_pts - pts < HOLD_MAX {
                return None;
            }
        }
        let rank = |a: &Option<(u16, u64, bool)>| {
            a.map_or((0, 0), |(_, pts, lost)| (u8::from(!lost), pts))
        };
        let pinned = self.pinned_anchor();
        let pending = |a: &Option<(u16, u64, bool)>| {
            a.is_some_and(|(_, pts, lost)| {
                !lost
                    && self.is_shared(pts)
                    && !self.is_common(pts)
                    && self.next_pts - pts < PENDING_PERIODS * self.anchor_period()
            })
        };
        (0..self.anchors.len())
            .filter(|&i| self.anchors.len() == 1 || Some(i) != pinned)
            .filter(|&i| wraps || !pending(&self.anchors[i]))
            .min_by_key(|&i| rank(&self.anchors[i]))
            .map(|i| i as u8)
    }

    /// Whether an anchor marked now would predict from an older one (`settle`): no recent frame
    /// held is one every consumer was sent, as while one of them lags, and the newest anchor is
    /// under `HOLD_MAX` frames old. A lone anchor so marked costs every consumer the bits of a
    /// prediction from an anchor period back, and the one it would mark over serves the one
    /// lagging as well.
    fn reaches_back(&self) -> bool {
        self.newest_valid()
            .is_some_and(|(_, pts)| !self.is_shared(pts))
            && !self.frames.iter().any(|f| !f.2 && self.is_shared(f.1))
            && self
                .anchor_frames()
                .filter(|a| !a.2)
                .map(|a| a.1)
                .max()
                .is_some_and(|pts| self.next_pts - pts < HOLD_MAX)
    }

    /// How many frames apart the schedule marks anchors.
    fn anchor_period(&self) -> u64 {
        if self.anchors.len() == 1 {
            ANCHOR_ALONE_EVERY
        } else {
            ANCHOR_EVERY
        }
    }

    /// Whether the next frame carries H.264 `frame_num` 0, the consumers acknowledging.
    fn wraps_next(&self) -> bool {
        self.acknowledging()
            && self.frame_num_range > 0
            && (self.next_pts - self.key_pts).is_multiple_of(self.frame_num_range)
    }

    /// The consumer says which frames its clients hold (`acknowledge`) from the first frame on.
    pub fn set_acknowledged(&mut self) {
        self.acknowledged = true;
    }

    /// Note that every consumer holds `frame_id`, or where not `held` was sent it; false for a
    /// frame not remembered, one a consumer reported lost, or one from before the last key frame.
    pub fn acknowledge(&mut self, frame_id: u16, held: bool) -> bool {
        self.acknowledged = true;
        let Some(&(_, pts, lost)) = self.recent.iter().rev().find(|f| f.0 == frame_id) else {
            return false;
        };
        if lost || pts < self.key_pts {
            return false;
        }
        let noted = if held {
            &mut self.common
        } else {
            &mut self.sent
        };
        if let Err(at) = noted.binary_search(&pts) {
            noted.insert(at, pts);
        }
        while noted.len() > RECENT_FRAMES {
            noted.pop_front();
        }
        true
    }

    /// Whether the consumers acknowledge what they hold or were sent (`set_acknowledged`).
    pub fn acknowledging(&self) -> bool {
        self.acknowledged
    }

    /// Whether every consumer holds the frame at `pts`: the key frame, which each is sent before
    /// any other, one acknowledged, or, acknowledged, an H.264 frame a consumer never leaves out
    /// (`kept`), so the anchor at the `frame_num` wrap stays pinned until a newer one is held.
    fn is_common(&self, pts: u64) -> bool {
        pts == self.key_pts
            || (self.acknowledged && self.kept(pts))
            || self.common.binary_search(&pts).is_ok()
    }

    /// Whether the frame at `pts` is one a consumer sends every client of an H.264 stream it
    /// sends any of (pixelflux's consumers do): where `frame_num` wraps, which a decoder cannot
    /// be predicted past without.
    fn kept(&self, pts: u64) -> bool {
        self.frame_num_range > 0
            && pts > self.key_pts
            && (pts - self.key_pts).is_multiple_of(self.frame_num_range)
    }

    /// Whether every consumer was sent the frame at `pts`, as every one holds a common one.
    fn is_shared(&self, pts: u64) -> bool {
        self.is_common(pts) || self.sent.binary_search(&pts).is_ok()
    }

    /// Whether every consumer was sent the frame the next one predicts from, asked before that
    /// one is recorded: a lone anchor marked over the one it predicts from lets that one go.
    pub fn predicts_from_shared(&self) -> bool {
        self.newest_valid()
            .is_some_and(|(_, pts)| self.is_shared(pts))
    }

    /// The anchor holding the newest frame every consumer holds, which the schedule leaves.
    fn pinned_anchor(&self) -> Option<usize> {
        if !self.acknowledged {
            return None;
        }
        self.anchors
            .iter()
            .enumerate()
            .filter_map(|(i, a)| a.filter(|f| !f.2 && self.is_common(f.1)).map(|f| (i, f.1)))
            .max_by_key(|&(_, pts)| pts)
            .map(|(i, _)| i)
    }

    /// Before an anchor: where the newest frame held is not one every consumer was sent, forget
    /// the frames after the newest that is, so the anchor predicts from it and every consumer can
    /// decode it. The invalidation that does it, for the encoder; None where none is needed, no
    /// frame held is one they were all sent, or that one lies further back than `SETTLE_SLACK`
    /// or, of two anchors, `SETTLE_RECENT` allows.
    pub fn settle(&mut self) -> Option<Invalidation> {
        if !self.acknowledging() {
            return None;
        }
        // The frame at a `frame_num` wrap predicts from one every consumer holds, not one it was
        // only sent: a client that lost that one could not be predicted past the wrap.
        let wraps = self.wraps_next();
        let usable = |w: &Self, pts: u64| {
            if wraps {
                w.is_common(pts)
            } else {
                w.is_shared(pts)
            }
        };
        let (_, newest) = self.newest_valid()?;
        if usable(self, newest) {
            return None;
        }
        let shared = self
            .held()
            .filter(|f| !f.2 && usable(self, f.1))
            .map(|f| f.1)
            .max()?;
        if !wraps && self.next_pts - shared > self.anchor_period() + SETTLE_SLACK {
            return None;
        }
        if self.anchors.len() > 1
            && !wraps
            && !self
                .frames
                .iter()
                .rev()
                .filter(|f| !f.2)
                .take(SETTLE_RECENT)
                .any(|f| f.1 == shared)
        {
            return None;
        }
        let &(first, _, _) = self.recent.iter().find(|f| f.1 > shared && !f.2)?;
        Some(self.invalidate(first))
    }

    /// Before an H.264 frame: where the newest frame held is a recent one further back than the
    /// buffer holds recent frames, forget the frames after the newest anchor every consumer was
    /// sent (acknowledged: the pinned one, a newer one they were all sent, or the one at the
    /// `frame_num` wrap, which a consumer never leaves out), else every recent one, so the frame
    /// predicts from an anchor. A decoder that fills a `frame_num` gap with pictures of its own
    /// (FFmpeg's, which Chromium decodes in software with, and Firefox and WebKit on Linux) has let
    /// that recent frame go by then for a client that missed the frames since (anchors among them,
    /// which the recent frames leave out), and fails a frame that names it (Chromium's does). The
    /// invalidation, for the encoder; None where none is needed.
    pub fn forget_stale(&mut self) -> Option<Invalidation> {
        if self.frame_num_range == 0 || self.anchors.is_empty() {
            return None;
        }
        let (_, newest) = self.newest_valid()?;
        let recent = self.frames.iter().any(|f| f.1 == newest && !f.2);
        if !recent || self.next_pts - newest <= self.capacity as u64 {
            return None;
        }
        let target = self
            .anchor_frames()
            .filter(|a| !a.2 && (!self.acknowledged || self.is_shared(a.1)))
            .map(|a| a.1)
            .max();
        let first = match target {
            Some(anchor) => self.recent.iter().find(|f| f.1 > anchor && !f.2)?.0,
            None => self.frames.iter().find(|f| !f.2)?.0,
        };
        Some(self.invalidate(first))
    }

    /// The anchor the next frame predicts from, where the newest frame held is an anchor; the
    /// encoder names it, since NVENC's H.264 fallback takes the first anchor otherwise.
    pub fn predicting_anchor(&self) -> Option<u8> {
        let (_, pts) = self.newest_valid()?;
        if self.frames.iter().any(|f| f.1 == pts && !f.2) {
            return None;
        }
        self.anchors
            .iter()
            .position(|a| a.is_some_and(|a| a.1 == pts && !a.2))
            .map(|i| i as u8)
    }

    /// The anchor holding the frame at `pts`, where one does, lost or not: an encoder that writes
    /// its own headers lists it long-term under that index while the decoder holds it.
    pub fn anchor_slot(&self, pts: u64) -> Option<u8> {
        self.anchors
            .iter()
            .position(|a| a.is_some_and(|a| a.1 == pts))
            .map(|i| i as u8)
    }

    /// How many values the stream's `frame_num` takes before it wraps; 0 for a codec without
    /// that counter.
    pub fn set_frame_num_range(&mut self, range: u32) {
        self.frame_num_range = range as u64;
    }

    /// Forget every frame held, for a stream the session restarts with a key frame of its own.
    pub fn reset(&mut self) {
        self.frames.clear();
        self.anchors.iter_mut().for_each(|a| *a = None);
        self.recent.clear();
        self.common.clear();
        self.sent.clear();
    }

    /// The decoded picture buffer the session has now; frames past it are let go. The anchors
    /// take their share of it.
    pub fn set_capacity(&mut self, capacity: u32) {
        self.capacity = capacity.saturating_sub(self.anchors.len() as u32).max(1) as usize;
        while self.frames.len() > self.recent_capacity() {
            self.frames.pop_front();
        }
    }

    /// The timestamp the next frame is encoded with.
    pub fn next_pts(&self) -> u64 {
        self.next_pts
    }

    /// The anchors held, in no order.
    fn anchor_frames(&self) -> impl Iterator<Item = (u16, u64, bool)> + '_ {
        self.anchors.iter().flatten().copied()
    }

    /// Whether a frame not coded as a key frame has a reference left to predict from.
    pub fn has_reference(&self) -> bool {
        self.frames
            .iter()
            .copied()
            .chain(self.anchor_frames())
            .any(|f| !f.2)
    }

    /// The newest frame the client still has, as its id and timestamp: the one the next frame
    /// predicts from.
    pub fn newest_valid(&self) -> Option<(u16, u64)> {
        let held = self.frames.iter().copied().chain(self.anchor_frames());
        held.filter(|f| !f.2)
            .max_by_key(|f| f.1)
            .map(|f| (f.0, f.1))
    }

    /// The frames the decoder holds, oldest first: each frame's id, timestamp, and whether
    /// a client reported it lost.
    pub fn held(&self) -> impl Iterator<Item = (u16, u64, bool)> + '_ {
        // The anchors, newest first so the oldest pops, merged into the recent frames.
        let mut anchors: Vec<_> = self.anchor_frames().collect();
        anchors.sort_by_key(|f| std::cmp::Reverse(f.1));
        let mut frames = self.frames.iter().copied().peekable();
        std::iter::from_fn(move || {
            if anchors
                .last()
                .is_some_and(|a| frames.peek().is_none_or(|f| a.1 < f.1))
            {
                anchors.pop()
            } else {
                frames.next()
            }
        })
    }

    /// The timestamp of the last key frame, where the counts a codec keeps restart.
    pub fn key_pts(&self) -> u64 {
        self.key_pts
    }

    /// Record the frame just encoded with `next_pts` and answer what it predicted from.
    pub fn record(&mut self, frame_id: u16, key: bool) -> Reference {
        self.record_marked(frame_id, key, None)
    }

    /// Record the frame just encoded with `next_pts`, marked into `anchor` where the session
    /// marked it (`plan_anchor`), and answer what it predicted from.
    pub fn record_marked(&mut self, frame_id: u16, key: bool, anchor: Option<u8>) -> Reference {
        let pts = self.next_pts;
        self.next_pts += 1;
        self.age.record(frame_id, key);
        let reference = if key {
            self.frames.clear();
            self.anchors.iter_mut().for_each(|a| *a = None);
            self.common.clear();
            self.sent.clear();
            self.key_pts = pts;
            Reference::None
        } else {
            self.newest_valid()
                .map_or(Reference::None, |(id, _)| Reference::Frame(id))
        };
        match anchor.and_then(|slot| self.anchors.get_mut(slot as usize)) {
            Some(slot) => *slot = Some((frame_id, pts, false)),
            None => self.frames.push_back((frame_id, pts, false)),
        }
        while self.frames.len() > self.recent_capacity() {
            self.frames.pop_front();
        }
        self.recent.push_back((frame_id, pts, false));
        if self.recent.len() > RECENT_FRAMES {
            self.recent.pop_front();
        }
        reference
    }

    /// Take back the frame just recorded, which the session codes again in its place under the
    /// same id: it is left out of the predictions as a frame the client lost is, and `forget`
    /// tells the encoder its timestamp. False, with the window as it was, where it was a key
    /// frame, nothing older is left to predict from, it carries an H.264 `frame_num` 0, which
    /// the decoder has to see (`set_frame_num_range`), or `forget` refuses.
    pub fn retract(&mut self, forget: impl FnOnce(u64) -> bool) -> bool {
        let Some(pts) = self.next_pts.checked_sub(1) else {
            return false;
        };
        let older = self
            .frames
            .iter()
            .copied()
            .chain(self.anchor_frames())
            .any(|f| !f.2 && f.1 < pts);
        let wraps =
            self.frame_num_range > 0 && (pts - self.key_pts).is_multiple_of(self.frame_num_range);
        if !older || wraps || !forget(pts) {
            return false;
        }
        let held = self
            .frames
            .iter_mut()
            .chain(self.anchors.iter_mut().flatten());
        for f in held.chain(self.recent.iter_mut()).filter(|f| f.1 == pts) {
            f.2 = true;
        }
        true
    }

    /// Leave `frame_id` and every frame encoded after it out of the references. A report for
    /// a picture already removed by an earlier invalidation leaves the recovered chain intact.
    /// The bounded history dates those reports across capture-id gaps and wrap; a loss older
    /// than the history keeps the conservative key-frame fallback.
    pub fn invalidate(&mut self, frame_id: u16) -> Invalidation {
        if self
            .recent
            .iter()
            .rev()
            .find(|f| f.0 == frame_id)
            .is_some_and(|f| f.2)
        {
            return Invalidation::Ignored;
        }
        let action = if self.anchored() {
            self.invalidate_anchored(frame_id)
        } else {
            self.invalidate_recent(frame_id)
        };
        let cut = match action {
            Invalidation::Forget(pts) => pts,
            Invalidation::KeyFrame => self.key_pts,
            Invalidation::Ignored => return action,
        };
        for f in self.recent.iter_mut().filter(|f| f.1 >= cut) {
            f.2 = true;
        }
        action
    }

    /// Invalidate a session with only recent references.
    fn invalidate_recent(&mut self, frame_id: u16) -> Invalidation {
        let Some(&(oldest, _, _)) = self.frames.front() else {
            return Invalidation::Ignored;
        };
        // The newest with the id: a retracted frame shares it with the one sent in its place.
        match self.frames.iter().rposition(|f| f.0 == frame_id) {
            Some(at) => {
                let pts = self.frames[at].1;
                let wraps = self.frame_num_range > 0
                    && self
                        .frames
                        .iter()
                        .skip(at)
                        .any(|f| (f.1 - self.key_pts).is_multiple_of(self.frame_num_range));
                for f in self.frames.iter_mut().skip(if wraps { 0 } else { at }) {
                    f.2 = true;
                }
                if wraps {
                    Invalidation::KeyFrame
                } else {
                    Invalidation::Forget(pts)
                }
            }
            None if self
                .age
                .behind(frame_id)
                .is_some_and(|b| b > self.age.newest.wrapping_sub(oldest) as u64) =>
            {
                for f in self.frames.iter_mut() {
                    f.2 = true;
                }
                Invalidation::KeyFrame
            }
            None => Invalidation::Ignored,
        }
    }

    /// `invalidate` where anchors are kept: a loss older than every recent frame is dated by
    /// the frames remembered and predicted past from an anchor before it, where one is left.
    fn invalidate_anchored(&mut self, frame_id: u16) -> Invalidation {
        let remembered = self
            .recent
            .iter()
            .rev()
            .find(|r| r.0 == frame_id)
            .map(|r| r.1);
        let lost_pts = match remembered {
            Some(pts) if pts >= self.key_pts => pts,
            Some(_) => return Invalidation::Ignored,
            None => {
                // Older than every frame remembered, so everything held predicts through it; a
                // frame before the key frame, or one never sent, was never a reference.
                let Some(&(oldest, _, _)) = self.recent.front() else {
                    return Invalidation::Ignored;
                };
                if !self
                    .age
                    .behind(frame_id)
                    .is_some_and(|b| b > self.age.newest.wrapping_sub(oldest) as u64)
                {
                    return Invalidation::Ignored;
                }
                self.key_pts
            }
        };
        let newest = self.next_pts.saturating_sub(1);
        // The first frame at or after the loss that carries `frame_num` 0, if one was sent.
        let range = self.frame_num_range;
        let wraps =
            range > 0 && (lost_pts - self.key_pts).div_ceil(range) * range <= newest - self.key_pts;
        let cut = if wraps { 0 } else { lost_pts };
        for f in self.frames.iter_mut().filter(|f| f.1 >= cut) {
            f.2 = true;
        }
        for a in self.anchors.iter_mut().flatten().filter(|a| a.1 >= cut) {
            a.2 = true;
        }
        if wraps || !self.has_reference() {
            Invalidation::KeyFrame
        } else {
            Invalidation::Forget(lost_pts)
        }
    }
}

/// The buffers a VP8 or VP9 encoder predicts from, with the frame each holds and whether a
/// client reported it lost.
///
/// Some buffers are spent on anchors older than the last few frames, so a loss up to twelve
/// frames deep still finds a frame older than it. VP8 has three, LAST, GOLDEN, and ALTREF: every
/// frame lands in LAST, and every twelfth in GOLDEN and ALTREF by turns, so one anchor is always
/// twelve to twenty-three frames old. VP9 has eight: the last four frames by turns in the first
/// four, and every fourth frame by turns in the other four, the oldest twelve to fifteen frames
/// back. A frame predicts from the newest buffer whose frame the client still has, the first
/// such buffer when others are as new, and a VP8 frame coded from an anchor rather than LAST
/// refreshes all three, since it is the newest picture both sides hold. The frame ids wrap; the
/// timestamps are the session's own count of encoded frames and do not, and the recent ones are
/// kept so a lost frame no buffer holds still dates the buffers coded after it.
pub struct ReferenceSlots {
    slots: [Option<(u16, u64, bool)>; 8],
    count: usize,
    recent: VecDeque<(u16, u64)>,
    next_pts: u64,
    age: KeyAge,
    key_pts: u64,
    acknowledged: bool,
    common: VecDeque<u64>,
    sent: VecDeque<u64>,
}

/// How many recent frames are remembered by id, about eight seconds at 60 frames a second: a
/// consumer whose clients hold a pinned anchor may report a run lost that long ago and have it
/// predicted past (`acknowledge`); the anchors alone reach twenty-three back.
const RECENT_FRAMES: usize = 512;

/// VP8's GOLDEN and ALTREF each take every `ANCHOR_PERIOD`th frame, half a period apart.
const ANCHOR_PERIOD: u64 = 24;

/// VP9 keeps the last `RING` frames, and every `RING`th frame in as many buffers again.
const RING: u64 = 4;

/// The buffers a frame refreshes, as a bit per slot: LAST, GOLDEN, ALTREF for VP8, the eight in
/// order for VP9.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SlotRefresh(pub u8);

impl SlotRefresh {
    pub const LAST: u8 = 1;
    pub const GOLDEN: u8 = 2;
    pub const ALTREF: u8 = 4;
    pub const ALL: u8 = 7;

    pub fn refreshes(self, slot: u8) -> bool {
        self.0 & slot != 0
    }
}

/// How the next frame predicts and which buffers it refreshes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SlotPlan {
    /// The buffer predicted from, as a `SlotRefresh` bit; zero for a key frame.
    pub predict_from: u8,
    pub refresh: SlotRefresh,
}

impl SlotPlan {
    /// A VP8 key frame: it predicts from nothing and re-anchors every buffer.
    pub const KEY: Self = Self {
        predict_from: 0,
        refresh: SlotRefresh(SlotRefresh::ALL),
    };
}

impl Default for ReferenceSlots {
    fn default() -> Self {
        Self::new()
    }
}

impl ReferenceSlots {
    /// VP8's three buffers.
    pub fn new() -> Self {
        Self::with(3)
    }

    /// VP9's eight buffers.
    pub fn vp9() -> Self {
        Self::with(8)
    }

    fn with(count: usize) -> Self {
        Self {
            slots: [None; 8],
            count,
            recent: VecDeque::new(),
            next_pts: 0,
            age: KeyAge::default(),
            key_pts: 0,
            acknowledged: false,
            common: VecDeque::new(),
            sent: VecDeque::new(),
        }
    }

    /// The timestamp the next frame is encoded with.
    pub fn next_pts(&self) -> u64 {
        self.next_pts
    }

    /// Note that every consumer holds `frame_id`, or where not `held` was sent it, as
    /// `ReferenceWindow::acknowledge` does: the anchor buffer holding the newest frame they all
    /// hold is then pinned, and one they were all sent is kept until they hold it
    /// (`PENDING_PERIODS`). False for a frame not remembered or from before the key frame.
    pub fn acknowledge(&mut self, frame_id: u16, held: bool) -> bool {
        self.acknowledged = true;
        let Some(&(_, pts)) = self.recent.iter().rev().find(|f| f.0 == frame_id) else {
            return false;
        };
        if pts < self.key_pts {
            return false;
        }
        let noted = if held {
            &mut self.common
        } else {
            &mut self.sent
        };
        if let Err(at) = noted.binary_search(&pts) {
            noted.insert(at, pts);
        }
        while noted.len() > RECENT_FRAMES {
            noted.pop_front();
        }
        true
    }

    fn is_common(&self, pts: u64) -> bool {
        pts == self.key_pts || self.common.binary_search(&pts).is_ok()
    }

    fn is_shared(&self, pts: u64) -> bool {
        self.is_common(pts) || self.sent.binary_search(&pts).is_ok()
    }

    /// The buffers holding anchors: VP8's GOLDEN and ALTREF, VP9's last `RING`.
    fn anchor_slots(&self) -> std::ops::Range<usize> {
        if self.count > 3 {
            RING as usize..self.count
        } else {
            1..3
        }
    }

    /// How many frames apart an acknowledged session's schedule marks an anchor.
    fn anchor_every(&self) -> u64 {
        if self.count > 3 {
            RING
        } else {
            ANCHOR_PERIOD / 2
        }
    }

    /// The anchor buffer an acknowledged session refreshes next, if any: an empty or lost one
    /// first, then the oldest, never the one holding the newest frame every consumer holds, nor
    /// one holding a frame they were all sent and do not hold yet, marked within
    /// `PENDING_PERIODS` anchor periods.
    fn anchor_target(&self) -> Option<usize> {
        let pinned = self
            .anchor_slots()
            .filter_map(|i| {
                self.slots[i]
                    .filter(|f| !f.2 && self.is_common(f.1))
                    .map(|f| (i, f.1))
            })
            .max_by_key(|&(_, pts)| pts)
            .map(|(i, _)| i);
        self.anchor_slots()
            .filter(|&i| Some(i) != pinned)
            .filter(|&i| {
                !self.slots[i].is_some_and(|(_, pts, lost)| {
                    !lost
                        && self.is_shared(pts)
                        && !self.is_common(pts)
                        && self.next_pts - pts < PENDING_PERIODS * self.anchor_every()
                })
            })
            .min_by_key(|&i| self.slots[i].map_or((0, 0), |(_, pts, lost)| (u8::from(!lost), pts)))
    }

    /// Whether a frame coded under `plan` is an anchor on the schedule predicting from a buffer
    /// holding a frame every consumer was sent, so each can decode it (`FRAME_ANCHOR`). One
    /// predicting past a loss is not flagged: a consumer it reaches takes it as the end of its
    /// run, while a flagged one would wait for room behind the consumer's queue.
    pub fn shared_anchor(&self, plan: SlotPlan) -> bool {
        plan.predict_from != 0
            && (self.next_pts - self.key_pts).is_multiple_of(self.anchor_every())
            && self.anchor_slots().any(|i| plan.refresh.refreshes(1 << i))
            && self
                .slot(plan.predict_from)
                .is_some_and(|(_, pts, lost)| !lost && self.is_shared(pts))
    }

    /// Whether a frame not coded as a key frame has a buffer left to predict from.
    pub fn has_reference(&self) -> bool {
        self.slots[..self.count]
            .iter()
            .any(|s| s.is_some_and(|(_, _, lost)| !lost))
    }

    /// What each buffer holds: the frame's id and timestamp and whether it was reported lost.
    pub fn slot(&self, slot: u8) -> Option<(u16, u64, bool)> {
        self.slots[slot.trailing_zeros() as usize]
    }

    /// The buffer the next frame predicts from (the newest one the client still has) and the
    /// buffers it refreshes; a key frame refreshes all of them and predicts from none.
    pub fn plan(&self, key: bool) -> SlotPlan {
        if key || !self.has_reference() {
            return SlotPlan {
                predict_from: 0,
                refresh: SlotRefresh(((1u16 << self.count) - 1) as u8),
            };
        }
        let mut newest = 0;
        for i in 0..self.count {
            if let Some((_, pts, false)) = self.slots[i]
                && self.slots[newest].is_none_or(|(_, held, lost)| lost || pts > held)
            {
                newest = i;
            }
        }
        let predict_from = 1 << newest;
        let since_key = self.next_pts - self.key_pts;
        if self.acknowledged {
            // A frame predicting past a loss from an anchor, the newest picture every consumer
            // can be given, is an anchor too, as on the schedule.
            let recent = if self.count > 3 {
                1 << (since_key % RING)
            } else {
                SlotRefresh::LAST
            };
            let anchor = since_key.is_multiple_of(self.anchor_every())
                || self.anchor_slots().contains(&newest);
            let target = if anchor { self.anchor_target() } else { None };
            return SlotPlan {
                predict_from,
                refresh: SlotRefresh(recent | target.map_or(0, |i| 1 << i)),
            };
        }
        let refresh = if self.count > 3 {
            1 << (since_key % RING)
                | if since_key.is_multiple_of(RING) {
                    1 << (RING + since_key / RING % RING)
                } else {
                    0
                }
        } else if predict_from != SlotRefresh::LAST {
            SlotRefresh::ALL
        } else {
            SlotRefresh::LAST
                | if since_key.is_multiple_of(ANCHOR_PERIOD) {
                    SlotRefresh::GOLDEN
                } else {
                    0
                }
                | if since_key % ANCHOR_PERIOD == ANCHOR_PERIOD / 2 {
                    SlotRefresh::ALTREF
                } else {
                    0
                }
        };
        SlotPlan {
            predict_from,
            refresh: SlotRefresh(refresh),
        }
    }

    /// Record the frame just encoded under `plan` and answer what it predicted from.
    pub fn record(&mut self, frame_id: u16, plan: SlotPlan) -> Reference {
        let pts = self.next_pts;
        self.next_pts += 1;
        self.age.record(frame_id, plan.predict_from == 0);
        let reference = match plan.predict_from {
            0 => {
                self.key_pts = pts;
                self.common.clear();
                self.sent.clear();
                Reference::None
            }
            from => self.slots[from.trailing_zeros() as usize]
                .map_or(Reference::None, |(id, _, _)| Reference::Frame(id)),
        };
        for (i, slot) in self.slots[..self.count].iter_mut().enumerate() {
            if plan.predict_from == 0 || plan.refresh.refreshes(1 << i) {
                *slot = Some((frame_id, pts, false));
            }
        }
        self.recent.push_back((frame_id, pts));
        if self.recent.len() > RECENT_FRAMES {
            self.recent.pop_front();
        }
        reference
    }

    /// Leave `frame_id` and every frame encoded after it out of the references.
    pub fn invalidate(&mut self, frame_id: u16) -> Invalidation {
        let lost_pts = match self.recent.iter().find(|r| r.0 == frame_id) {
            Some(&(_, pts)) if pts >= self.key_pts => pts,
            Some(_) => return Invalidation::Ignored,
            None => {
                // A frame older than what is remembered was sent before every buffered one,
                // so everything held predicts through it; one before the key frame, or a
                // newer one, was never a reference.
                let Some(&(oldest, _)) = self.recent.front() else {
                    return Invalidation::Ignored;
                };
                if !self
                    .age
                    .behind(frame_id)
                    .is_some_and(|b| b > self.age.newest.wrapping_sub(oldest) as u64)
                {
                    return Invalidation::Ignored;
                }
                0
            }
        };
        for slot in self.slots[..self.count].iter_mut().flatten() {
            if slot.1 >= lost_pts {
                slot.2 = true;
            }
        }
        if self.has_reference() {
            Invalidation::Forget(lost_pts)
        } else {
            Invalidation::KeyFrame
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_frame_predicts_from_the_newest_one_the_client_still_has() {
        let mut w = ReferenceWindow::new(4);
        assert!(
            !w.has_reference(),
            "an empty window has nothing to predict from"
        );
        assert_eq!(w.record(10, true), Reference::None);
        assert_eq!(w.record(11, false), Reference::Frame(10));
        assert_eq!(w.record(12, false), Reference::Frame(11));
        assert_eq!(w.invalidate(11), Invalidation::Forget(1));
        assert!(w.has_reference(), "frame 10 is still there to predict from");
        assert_eq!(w.record(13, false), Reference::Frame(10));
        assert_eq!(w.record(14, false), Reference::Frame(13));
        assert_eq!(w.next_pts(), 5);
    }

    #[test]
    fn losing_every_reference_asks_for_a_key_frame() {
        let mut w = ReferenceWindow::new(3);
        w.record(0, true);
        for id in 1..6u16 {
            w.record(id, false);
        }
        // The window holds 3, 4, and 5; frame 2 has left it, and everything held predicts
        // through it.
        assert_eq!(w.invalidate(2), Invalidation::KeyFrame);
        assert!(!w.has_reference());
        assert_eq!(w.record(6, true), Reference::None);
        assert!(w.has_reference());
        // Forgetting the only frame held leaves nothing either.
        assert_eq!(w.invalidate(6), Invalidation::Forget(6));
        assert!(!w.has_reference());
    }

    #[test]
    fn a_frame_older_than_the_key_frame_is_ignored() {
        let mut w = ReferenceWindow::new(4);
        w.record(100, true);
        w.record(101, false);
        assert_eq!(w.invalidate(99), Invalidation::Ignored);
        assert_eq!(
            w.invalidate(105),
            Invalidation::Ignored,
            "a frame never sent is nothing to forget"
        );
        assert!(w.has_reference());
    }

    #[test]
    fn frame_ids_wrap_and_timestamps_do_not() {
        let mut w = ReferenceWindow::new(4);
        w.record(65534, true);
        w.record(65535, false);
        w.record(0, false);
        assert_eq!(w.record(1, false), Reference::Frame(0));
        assert_eq!(w.invalidate(65535), Invalidation::Forget(1));
        assert_eq!(w.record(2, false), Reference::Frame(65534));
        assert_eq!(
            w.invalidate(65533),
            Invalidation::Ignored,
            "before the key frame"
        );
        w.set_capacity(2);
        assert_eq!(
            w.invalidate(65534),
            Invalidation::KeyFrame,
            "left the window, and everything held predicts through it"
        );
    }

    #[test]
    fn a_loss_covering_the_frame_num_wrap_costs_a_key_frame() {
        let mut w = ReferenceWindow::new(8);
        w.set_frame_num_range(16);
        w.record(0, true);
        for id in 1..=17u16 {
            w.record(id, false);
        }
        // Frame 16 carries frame_num 0 again; a loss that leaves it out cannot be predicted past.
        assert_eq!(w.invalidate(17), Invalidation::Forget(17));
        assert_eq!(w.record(18, false), Reference::Frame(16));
        assert_eq!(
            w.invalidate(15),
            Invalidation::KeyFrame,
            "15, 16, and 18 go, and 16 is the wrap"
        );
        assert!(!w.has_reference());
        assert_eq!(w.record(19, true), Reference::None);
        assert_eq!(w.record(20, false), Reference::Frame(19));
        assert_eq!(
            w.invalidate(20),
            Invalidation::Forget(20),
            "the count restarts at the key frame"
        );
        let mut w = ReferenceWindow::new(8);
        w.record(0, true);
        for id in 1..=17u16 {
            w.record(id, false);
        }
        assert_eq!(
            w.invalidate(16),
            Invalidation::Forget(16),
            "a codec without the counter predicts past it"
        );
    }

    #[test]
    fn a_frame_coded_again_predicts_past_the_first_attempt() {
        let mut w = ReferenceWindow::new(4);
        assert!(!w.retract(|_| true), "nothing recorded");
        w.record(10, true);
        assert!(
            !w.retract(|_| panic!("a key frame is coded again as a key frame")),
            "nothing older than a key frame"
        );
        w.record(11, false);
        w.record(12, false);
        assert!(!w.retract(|_| false), "the encoder refused");
        assert_eq!(w.record(13, false), Reference::Frame(12));
        let mut told = None;
        assert!(w.retract(|pts| told.replace(pts).is_none()));
        assert_eq!(told, Some(3));
        assert_eq!(
            w.record(13, false),
            Reference::Frame(12),
            "the frame sent in its place predicts from the one before"
        );
        assert_eq!(w.record(14, false), Reference::Frame(13));
        assert_eq!(
            w.invalidate(13),
            Invalidation::Forget(4),
            "a loss of 13 is the frame sent"
        );
        assert_eq!(w.record(15, false), Reference::Frame(12));

        let mut w = ReferenceWindow::with_anchors(5, 2);
        let a = w.plan_anchor(true);
        w.record_marked(0, true, a);
        for id in 1..12u16 {
            let a = w.plan_anchor(false);
            w.record_marked(id, false, a);
        }
        let a = w.plan_anchor(false);
        assert_eq!(a, Some(1), "frame 12 is an anchor");
        w.record_marked(12, false, a);
        assert!(w.retract(|pts| pts == 12));
        assert_eq!(w.record_marked(12, false, a), Reference::Frame(11));
        assert_eq!(w.record(13, false), Reference::Frame(12));
        assert_eq!(w.invalidate(12), Invalidation::Forget(13));
        assert_eq!(w.record(14, false), Reference::Frame(11));

        let mut w = ReferenceWindow::new(8);
        w.set_frame_num_range(16);
        w.record(0, true);
        for id in 1..=15u16 {
            w.record(id, false);
        }
        assert!(w.retract(|_| true));
        w.record(15, false);
        assert!(
            !w.retract(|_| panic!("frame_num 0 has to reach the decoder")),
            "the frame sent in place of 15 carries frame_num 0"
        );
    }

    #[test]
    fn three_buffers_anchor_older_frames() {
        let mut s = ReferenceSlots::new();
        assert!(!s.has_reference());
        let key = s.plan(true);
        assert_eq!(key.predict_from, 0);
        assert_eq!(s.record(0, key), Reference::None);
        for id in 1..=30u16 {
            let plan = s.plan(false);
            assert_eq!(plan.predict_from, SlotRefresh::LAST, "frame {id}");
            assert_eq!(s.record(id, plan), Reference::Frame(id - 1));
        }
        // Frames 25 to 30 are reported lost: LAST holds 30, GOLDEN 24, ALTREF 12.
        assert_eq!(s.invalidate(25), Invalidation::Forget(25));
        let plan = s.plan(false);
        assert_eq!(plan.predict_from, SlotRefresh::GOLDEN);
        assert_eq!(
            plan.refresh,
            SlotRefresh(SlotRefresh::ALL),
            "a recovery frame re-anchors every buffer"
        );
        assert_eq!(s.record(31, plan), Reference::Frame(24));
        assert_eq!(s.record(32, s.plan(false)), Reference::Frame(31));
        // Losing the re-anchoring frame leaves nothing older than it.
        assert_eq!(s.invalidate(31), Invalidation::KeyFrame);
        assert!(!s.has_reference());
        assert_eq!(s.record(33, s.plan(false)), Reference::None);
        assert_eq!(
            s.invalidate(3),
            Invalidation::Ignored,
            "before the key frame"
        );
        assert_eq!(s.next_pts(), 34);
    }

    #[test]
    fn golden_and_altref_follow_their_periods() {
        let mut s = ReferenceSlots::new();
        s.record(0, s.plan(true));
        let mut refreshes = Vec::new();
        for id in 1..=48u16 {
            let plan = s.plan(false);
            refreshes.push(plan.refresh.0);
            s.record(id, plan);
        }
        for (i, &r) in refreshes.iter().enumerate() {
            let frame = i + 1;
            let want = SlotRefresh::LAST
                | if frame % 24 == 0 {
                    SlotRefresh::GOLDEN
                } else {
                    0
                }
                | if frame % 24 == 12 {
                    SlotRefresh::ALTREF
                } else {
                    0
                };
            assert_eq!(r, want, "frame {frame}");
        }
        // A loss of frames 37 to 48 takes the golden of frame 48; the altref of frame 36 is
        // older than it, though no buffer holds frame 37 itself.
        assert_eq!(s.invalidate(37), Invalidation::Forget(37));
        assert_eq!(s.plan(false).predict_from, SlotRefresh::ALTREF);
        assert_eq!(s.record(49, s.plan(false)), Reference::Frame(36));
        assert_eq!(
            s.invalidate(100),
            Invalidation::Ignored,
            "a frame never sent is nothing to forget"
        );
    }

    #[test]
    fn eight_buffers_keep_four_recent_frames_and_every_fourth() {
        let mut s = ReferenceSlots::vp9();
        let key = s.plan(true);
        assert_eq!(
            key.refresh,
            SlotRefresh(0xff),
            "a key frame refreshes all eight"
        );
        s.record(0, key);
        for id in 1..=19u16 {
            let plan = s.plan(false);
            let n = id as u8;
            let want = 1 << (n % 4)
                | if n.is_multiple_of(4) {
                    1 << (4 + n / 4 % 4)
                } else {
                    0
                };
            assert_eq!(plan.refresh.0, want, "frame {id}");
            assert_eq!(s.record(id, plan), Reference::Frame(id - 1));
        }
        let held: Vec<u16> = (0..8).map(|i| s.slot(1 << i).unwrap().0).collect();
        assert_eq!(held, [16, 17, 18, 19, 16, 4, 8, 12]);
        // Frames 9 to 19 are reported lost: the anchor of frame 8 is the newest older than it.
        assert_eq!(s.invalidate(9), Invalidation::Forget(9));
        let plan = s.plan(false);
        assert_eq!(plan.predict_from, 1 << 6);
        assert_eq!(s.record(20, plan), Reference::Frame(8));
        assert_eq!(s.record(21, s.plan(false)), Reference::Frame(20));
    }

    #[test]
    fn acknowledged_slots_keep_an_anchor_until_every_consumer_holds_it() {
        // VP8, every consumer sent each frame and holding the key frame alone: GOLDEN takes 12 and
        // keeps it, ALTREF keeping the key frame they hold, until 12 is four anchor periods old.
        let mut w = ReferenceSlots::new();
        w.record(0, w.plan(true));
        assert!(w.acknowledge(0, true));
        let mut marks = Vec::new();
        for id in 1..=72u16 {
            let plan = w.plan(false);
            if plan.refresh.0 & (SlotRefresh::GOLDEN | SlotRefresh::ALTREF) != 0 {
                marks.push((id, plan.refresh.0, w.shared_anchor(plan)));
            }
            w.record(id, plan);
            assert!(w.acknowledge(id, false));
        }
        let golden = SlotRefresh::LAST | SlotRefresh::GOLDEN;
        assert_eq!(marks, [(12, golden, true), (60, golden, true)]);
        // Once they hold 60 it is pinned, and the schedule takes ALTREF.
        assert!(w.acknowledge(60, true));
        let plan = w.plan(false);
        assert_eq!(plan.refresh.0, SlotRefresh::LAST);
        for id in 73..=84u16 {
            let plan = w.plan(false);
            if id == 84 {
                assert_eq!(plan.refresh.0, SlotRefresh::LAST | SlotRefresh::ALTREF);
            }
            w.record(id, plan);
        }
        // A frame predicting past a loss from an anchor is an anchor too, unflagged off the
        // schedule.
        w.invalidate(80);
        let plan = w.plan(false);
        assert_eq!(plan.predict_from, SlotRefresh::GOLDEN);
        assert_eq!(plan.refresh.0, SlotRefresh::LAST | SlotRefresh::ALTREF);
        assert!(!w.shared_anchor(plan));
    }

    /// A loss up to twelve frames deep, at any point of the schedule, still finds a buffer
    /// holding a frame older than it.
    #[test]
    fn a_loss_up_to_twelve_frames_deep_finds_an_anchor() {
        for make in [
            ReferenceSlots::new as fn() -> ReferenceSlots,
            ReferenceSlots::vp9,
        ] {
            for sent in 1..=60u16 {
                for depth in 1..=sent.min(12) {
                    let mut s = make();
                    s.record(0, s.plan(true));
                    for id in 1..=sent {
                        let plan = s.plan(false);
                        s.record(id, plan);
                    }
                    let lost = sent + 1 - depth;
                    assert!(
                        matches!(s.invalidate(lost), Invalidation::Forget(_)),
                        "{} buffers, frame {lost} of {sent} lost",
                        s.count
                    );
                    let plan = s.plan(false);
                    assert!(
                        s.slot(plan.predict_from)
                            .is_some_and(|(id, _, lost_too)| id < lost && !lost_too)
                    );
                }
            }
        }
    }

    #[test]
    fn the_set_held_leaves_out_what_a_client_lost() {
        let held = |w: &ReferenceWindow| w.held().filter(|f| !f.2).map(|f| f.1).collect::<Vec<_>>();
        let mut w = ReferenceWindow::new(4);
        assert!(
            held(&w).is_empty(),
            "nothing is held before the first frame"
        );
        w.record(10, true);
        w.record(11, false);
        w.record(12, false);
        assert_eq!(held(&w), [0, 1, 2]);
        assert_eq!(w.invalidate(11), Invalidation::Forget(1));
        assert_eq!(held(&w), [0], "11 and every frame after it are out");
        // The frame recorded next predicts from the newest one held, which is the set's last.
        assert_eq!(w.record(13, false), Reference::Frame(10));
        assert_eq!(held(&w), [0, 3]);
        // A lost frame still takes its place in the window until it is let go.
        w.record(14, false);
        assert_eq!(
            held(&w),
            [3, 4],
            "10 left the window of four, behind the two lost ones"
        );
        assert_eq!(w.invalidate(10), Invalidation::KeyFrame);
        assert!(held(&w).is_empty(), "the next frame has to be a key frame");
        w.record(15, true);
        assert_eq!(held(&w), [5]);
    }

    /// A loss is answered however long the stream has run since its key frame: the ids wrap
    /// every 65536 frames, so past half of that an id no longer orders a frame against the key,
    /// and a capture that skips unchanged frames leaves gaps between the ids it encodes.
    #[test]
    fn a_loss_long_after_the_key_frame_is_still_answered() {
        let mut w = ReferenceWindow::new(8);
        let mut s = ReferenceSlots::new();
        let id = |n: u32| (n * 3) as u16;
        w.record(id(0), true);
        s.record(id(0), SlotPlan::KEY);
        for n in 1..=40_000u32 {
            w.record(id(n), false);
            let plan = s.plan(false);
            s.record(id(n), plan);
        }
        assert_eq!(w.invalidate(id(39_998)), Invalidation::Forget(39_998));
        assert_eq!(s.invalidate(id(39_998)), Invalidation::Forget(39_998));
        assert_eq!(
            w.invalidate(id(39_900)),
            Invalidation::KeyFrame,
            "sent since the key, and older than the window"
        );
        assert_eq!(
            w.invalidate(id(39_999) + 1),
            Invalidation::Ignored,
            "a frame the capture skipped was never sent"
        );
        let mut w = ReferenceWindow::new(8);
        w.record(id(0), false);
        for n in 1..=40_000u32 {
            w.record(id(n), n == 39_990);
        }
        assert_eq!(
            w.invalidate(id(39_989)),
            Invalidation::Ignored,
            "before the key frame"
        );
        assert_eq!(w.invalidate(id(39_995)), Invalidation::Forget(39_995));
    }

    /// A window over four frames that keeps anchors, fed a key frame and `n` frames after it as
    /// its session would mark them.
    fn anchored(n: u16) -> ReferenceWindow {
        let mut w = ReferenceWindow::with_anchors(4, 2);
        for id in 0..=n {
            let key = id == 0;
            let slot = w.plan_anchor(key);
            w.record_marked(id, key, slot);
        }
        w
    }

    #[test]
    fn anchors_are_marked_every_twelfth_frame_into_the_older_one() {
        let mut w = ReferenceWindow::with_anchors(4, 2);
        assert_eq!(
            w.plan_anchor(true),
            Some(0),
            "the key frame is the first anchor"
        );
        assert_eq!(w.record_marked(0, true, Some(0)), Reference::None);
        let mut marks = Vec::new();
        for id in 1..=48u16 {
            let slot = w.plan_anchor(false);
            if let Some(slot) = slot {
                marks.push((id, slot));
            }
            assert_eq!(
                w.record_marked(id, false, slot),
                Reference::Frame(id - 1),
                "frame {id}"
            );
        }
        assert_eq!(marks, [(12, 1), (24, 0), (36, 1), (48, 0)]);
        // Two of the four frames are anchors, so two recent frames are held beside them.
        assert_eq!(w.held().map(|f| f.0).collect::<Vec<_>>(), [36, 46, 47, 48]);
    }

    #[test]
    fn one_anchor_takes_every_forty_eighth_frame() {
        let mut w = ReferenceWindow::with_anchors(4, 1);
        w.record_marked(0, true, w.plan_anchor(true));
        let mut marks = Vec::new();
        for id in 1..=100u16 {
            let slot = w.plan_anchor(false);
            if let Some(slot) = slot {
                marks.push((id, slot));
            }
            w.record_marked(id, false, slot);
        }
        assert_eq!(marks, [(48, 0), (96, 0)]);
        // Three recent frames beside the anchor; a loss before the anchor finds nothing older.
        assert_eq!(w.held().map(|f| f.0).collect::<Vec<_>>(), [96, 98, 99, 100]);
        assert_eq!(w.invalidate(97), Invalidation::Forget(97));
        assert_eq!(
            w.record_marked(101, false, w.plan_anchor(false)),
            Reference::Frame(96)
        );
        assert_eq!(w.invalidate(95), Invalidation::KeyFrame);
    }

    #[test]
    fn a_loss_older_than_every_recent_frame_is_predicted_past_from_an_anchor() {
        // Held: anchors 0 and 12, recent frames 19 and 20.
        let mut w = anchored(20);
        assert_eq!(w.invalidate(16), Invalidation::Forget(16));
        assert_eq!(
            w.record_marked(21, false, w.plan_anchor(false)),
            Reference::Frame(12)
        );
        assert_eq!(
            w.record_marked(22, false, w.plan_anchor(false)),
            Reference::Frame(21)
        );
        // A loss before the newer anchor leaves the key frame.
        let mut w = anchored(20);
        assert_eq!(w.invalidate(10), Invalidation::Forget(10));
        assert_eq!(
            w.record_marked(21, false, w.plan_anchor(false)),
            Reference::Frame(0)
        );
        // So does a loss of the newer anchor itself.
        let mut w = anchored(20);
        assert_eq!(w.invalidate(12), Invalidation::Forget(12));
        assert_eq!(w.newest_valid(), Some((0, 0)));
        // Losing the key frame leaves nothing.
        let mut w = anchored(20);
        assert_eq!(w.invalidate(0), Invalidation::KeyFrame);
        assert!(!w.has_reference());
        assert_eq!(
            w.plan_anchor(false),
            Some(0),
            "the next frame is a key frame and the first anchor"
        );
    }

    #[test]
    fn a_lost_anchor_is_the_one_marked_next() {
        // Anchors 24 and 12; frame 24 is reported lost at frame 30.
        let mut w = anchored(30);
        assert_eq!(w.invalidate(24), Invalidation::Forget(24));
        assert_eq!(
            w.record_marked(31, false, w.plan_anchor(false)),
            Reference::Frame(12)
        );
        for id in 32..=35u16 {
            w.record_marked(id, false, w.plan_anchor(false));
        }
        // Frame 36 replaces the lost anchor, keeping frame 12, the one the client still has.
        assert_eq!(w.plan_anchor(false), Some(0));
    }

    #[test]
    fn the_wrap_rule_and_unsent_frames_hold_with_anchors() {
        let mut w = ReferenceWindow::with_anchors(4, 2);
        w.set_frame_num_range(16);
        for id in 0..=17u16 {
            let key = id == 0;
            let slot = w.plan_anchor(key);
            w.record_marked(id, key, slot);
        }
        // Frame 16 carries frame_num 0 again.
        assert_eq!(w.invalidate(17), Invalidation::Forget(17));
        assert_eq!(
            w.invalidate(15),
            Invalidation::KeyFrame,
            "15 to 17 go, and 16 is the wrap"
        );
        let mut w = anchored(20);
        assert_eq!(
            w.invalidate(40),
            Invalidation::Ignored,
            "a frame never sent is nothing to forget"
        );
        w.record_marked(21, true, Some(0));
        assert_eq!(
            w.invalidate(20),
            Invalidation::Ignored,
            "before the key frame"
        );
        assert_eq!(
            w.invalidate(21),
            Invalidation::KeyFrame,
            "the key frame was all there was"
        );
    }

    #[test]
    fn a_key_frame_the_session_forces_itself_is_marked() {
        let mut w = anchored(20);
        // A reconfigure that forces a key frame of its own resets the window first.
        w.reset();
        assert!(!w.has_reference(), "the next frame has to be a key frame");
        assert_eq!(w.plan_anchor(false), Some(0));
        assert_eq!(w.invalidate(19), Invalidation::Ignored, "nothing is held");
        // A key frame nobody marked leaves no anchor, so the next frame takes the first.
        let mut w = anchored(20);
        w.record_marked(21, true, None);
        assert_eq!(w.plan_anchor(false), Some(0));
        w.record_marked(22, false, Some(0));
        assert_eq!(w.plan_anchor(false), None);
    }

    #[test]
    fn anchors_take_their_share_of_the_buffer() {
        let mut w = ReferenceWindow::with_anchors(8, 2);
        for id in 0..=30u16 {
            let key = id == 0;
            let slot = w.plan_anchor(key);
            w.record_marked(id, key, slot);
        }
        assert_eq!(w.held().count(), 8, "six recent frames and two anchors");
        w.set_capacity(4);
        assert_eq!(w.held().count(), 4, "two recent frames and two anchors");
        assert!(!ReferenceWindow::new(4).anchored() && w.anchored());
    }

    #[test]
    fn the_wire_form_names_the_frame() {
        assert_eq!(Reference::Untracked.frame_id(), -2);
        assert_eq!(Reference::None.frame_id(), -1);
        assert_eq!(Reference::Frame(7).frame_id(), 7);
    }

    /// Different reports from one invalidated interval must not discard its repaired chain.
    #[test]
    fn covered_losses_preserve_recovered_references() {
        for (anchors, lost, last, recovered, later, fresh, earlier) in [
            (0, 6u16, 7u16, 14u16, 7u16, 13u16, 5u16),
            (1, 80, 87, 98, 81, 97, 40),
            (2, 26, 31, 50, 27, 49, 20),
        ] {
            for report in [later, fresh, earlier] {
                let mut w = if anchors == 0 {
                    ReferenceWindow::new(4)
                } else {
                    ReferenceWindow::with_anchors(8, anchors)
                };
                for id in 0..=last {
                    let key = id == 0;
                    w.record_marked(id, key, w.plan_anchor(key));
                }
                assert!(matches!(w.invalidate(lost), Invalidation::Forget(_)));
                for id in last + 1..=recovered {
                    w.record_marked(id, false, w.plan_anchor(false));
                }
                let valid = w.newest_valid();
                let action = w.invalidate(report);
                if report == later {
                    assert_eq!(action, Invalidation::Ignored, "{anchors} anchors");
                    assert_eq!(w.newest_valid(), valid);
                    assert_eq!(
                        w.record_marked(recovered + 1, false, w.plan_anchor(false)),
                        Reference::Frame(recovered)
                    );
                } else {
                    assert_ne!(action, Invalidation::Ignored, "loss {report} is new");
                }
            }
        }
    }

    /// Separate recovery intervals remain independent, including the frames between them.
    #[test]
    fn covered_loss_intervals_do_not_hide_intervening_losses() {
        let mut w = ReferenceWindow::new(4);
        for id in 0..=7 {
            w.record(id, id == 0);
        }
        assert_eq!(w.invalidate(6), Invalidation::Forget(6));
        for id in 8..=13 {
            w.record(id, false);
        }
        assert_eq!(w.invalidate(12), Invalidation::Forget(12));
        for id in 14..=18 {
            w.record(id, false);
        }
        for id in [6, 7, 12, 13] {
            assert_eq!(w.invalidate(id), Invalidation::Ignored);
            assert_eq!(w.newest_valid(), Some((18, 18)));
        }
        assert_eq!(w.invalidate(10), Invalidation::KeyFrame);
        assert_eq!(w.invalidate(13), Invalidation::Ignored);
        assert!(!w.has_reference(), "a required key frame stays required");
    }

    /// The record dates encoded frames rather than assuming consecutive capture ids.
    #[test]
    fn covered_loss_history_handles_sparse_wrapping_ids_and_expiry() {
        let id = |n: u32| (65520 + n * 3) as u16;
        let mut w = ReferenceWindow::with_anchors(4, 1);
        for n in 0..=87 {
            w.record_marked(id(n), n == 0, w.plan_anchor(n == 0));
        }
        assert_eq!(w.invalidate(id(80)), Invalidation::Forget(80));
        for n in 88..=98 {
            w.record_marked(id(n), false, w.plan_anchor(false));
        }
        assert_eq!(w.invalidate(id(81)), Invalidation::Ignored);
        assert_eq!(w.newest_valid(), Some((id(98), 98)));
        assert_eq!(w.invalidate(id(81) + 1), Invalidation::Ignored);
        for n in 99..=(98 + RECENT_FRAMES as u32) {
            w.record_marked(id(n), false, w.plan_anchor(false));
        }
        assert_eq!(w.invalidate(id(81)), Invalidation::KeyFrame);
    }

    /// Key frames and capture resets must not inherit a prior loss for a reused id.
    #[test]
    fn covered_loss_history_does_not_outlive_a_reused_frame_id() {
        for reset in [false, true] {
            let mut w = ReferenceWindow::new(4);
            for id in 0..=7 {
                w.record(id, id == 0);
            }
            assert_eq!(w.invalidate(6), Invalidation::Forget(6));
            w.record(8, false);
            if reset {
                w.reset();
            }
            w.record(6, true);
            assert_eq!(w.invalidate(6), Invalidation::Forget(9));
            assert!(!w.has_reference());
        }
    }

    /// A covered report before a frame_num wrap is harmless, but a newly lost wrap is not.
    #[test]
    fn covered_loss_keeps_the_h264_wrap_guard() {
        let mut w = ReferenceWindow::with_anchors(8, 1);
        w.set_frame_num_range(16);
        for id in 0..=14 {
            w.record_marked(id, id == 0, w.plan_anchor(id == 0));
        }
        assert_eq!(w.invalidate(13), Invalidation::Forget(13));
        for id in 15..=17 {
            w.record_marked(id, false, w.plan_anchor(false));
        }
        w.set_capacity(4);
        assert_eq!(w.invalidate(14), Invalidation::Ignored);
        assert_eq!(w.newest_valid(), Some((17, 17)));
        assert_eq!(w.invalidate(15), Invalidation::KeyFrame);
        assert!(!w.has_reference());
    }

    /// Feed frames to a window as a session does: plan the anchor, settle it, record it.
    fn feed(
        w: &mut ReferenceWindow,
        ids: std::ops::RangeInclusive<u16>,
    ) -> Vec<(u16, Option<u8>, Reference)> {
        ids.map(|id| {
            let key = id == 0;
            let slot = w.plan_anchor(key);
            if slot.is_some() && !key {
                w.settle();
            }
            (id, slot, w.record_marked(id, key, slot))
        })
        .collect()
    }

    #[test]
    fn an_acknowledged_anchor_is_pinned_while_the_schedule_marks_the_other() {
        let mut w = ReferenceWindow::with_anchors(4, 2);
        feed(&mut w, 0..=0);
        assert!(w.acknowledge(0, true));
        let marks: Vec<(u16, u8)> = feed(&mut w, 1..=48)
            .into_iter()
            .filter_map(|(id, slot, _)| slot.map(|s| (id, s)))
            .collect();
        assert_eq!(
            marks,
            [(12, 1), (24, 1), (36, 1), (48, 1)],
            "the key frame stays in slot 0"
        );
        assert!(w.acknowledge(48, true));
        assert_eq!(
            feed(&mut w, 49..=60).last().map(|f| f.1),
            Some(Some(0)),
            "48 pinned, the schedule takes slot 0 back"
        );
        assert!(!w.acknowledge(1000, true), "a frame never sent is not held");
    }

    #[test]
    fn an_anchor_predicts_from_the_newest_frame_every_consumer_was_sent() {
        let mut w = ReferenceWindow::with_anchors(8, 2);
        feed(&mut w, 0..=11);
        for id in 0..=10u16 {
            assert!(w.acknowledge(id, true));
        }
        assert_eq!(w.plan_anchor(false), Some(1));
        assert_eq!(
            w.settle(),
            Some(Invalidation::Forget(11)),
            "11 is forgotten"
        );
        assert!(w.predicts_from_shared());
        assert_eq!(w.record_marked(12, false, Some(1)), Reference::Frame(10));
        assert_eq!(
            w.record(13, false),
            Reference::Frame(12),
            "the stream goes on from the anchor"
        );

        // Of two anchors, one is not settled further back than the two newest recent frames:
        // it predicts from the frame before it.
        let mut w = ReferenceWindow::with_anchors(8, 2);
        feed(&mut w, 0..=11);
        for id in 0..=9u16 {
            assert!(w.acknowledge(id, true));
        }
        assert_eq!(w.settle(), None, "9 is three frames back");
        assert_eq!(w.record_marked(12, false, Some(1)), Reference::Frame(11));

        // Held by no consumer but the key frame, the other anchor: not settled on it either.
        let mut w = ReferenceWindow::with_anchors(4, 2);
        feed(&mut w, 0..=11);
        assert!(w.acknowledge(0, true));
        assert_eq!(w.settle(), None);
        assert_eq!(w.record_marked(12, false, Some(1)), Reference::Frame(11));

        // A lone anchor is not marked while it would predict from the one it marks over, every
        // consumer holding frames since that the window let go: they all hold that one.
        let mut w = ReferenceWindow::with_anchors(8, 1);
        feed(&mut w, 0..=47);
        for id in 0..=30u16 {
            assert!(w.acknowledge(id, true));
        }
        assert_eq!(w.plan_anchor(false), None);
        assert_eq!(w.record_marked(48, false, None), Reference::Frame(47));

        // Every consumer was sent the newest frame, which none holds yet: nothing to settle, and
        // nothing pinned on it.
        let mut w = ReferenceWindow::with_anchors(4, 2);
        feed(&mut w, 0..=11);
        assert!(w.acknowledge(0, true));
        assert!(w.acknowledge(11, false));
        assert_eq!(w.settle(), None);
        assert!(w.predicts_from_shared());
        let mut w = ReferenceWindow::with_anchors(4, 2);
        feed(&mut w, 0..=11);
        assert_eq!(
            w.settle(),
            None,
            "unacknowledged, an anchor is settled by nothing"
        );
    }

    #[test]
    fn only_an_anchor_every_consumer_holds_is_pinned() {
        // Sent to every consumer as it is marked, 12 is not pinned until all hold it, and the
        // schedule keeps it meanwhile: 24 is not marked over it, the key frame they hold staying.
        let mut w = ReferenceWindow::with_anchors(4, 2);
        feed(&mut w, 0..=0);
        assert!(w.acknowledge(0, true));
        for id in 1..=24u16 {
            feed(&mut w, id..=id);
            assert!(w.acknowledge(id, false));
        }
        let anchors: Vec<u16> = w.anchor_frames().map(|a| a.0).collect();
        assert!(anchors.contains(&0) && anchors.contains(&12), "{anchors:?}");
        assert!(w.acknowledge(12, true));
        assert_eq!(
            feed(&mut w, 25..=36).last().map(|f| f.1),
            Some(Some(0)),
            "12 pinned"
        );
        // 36, sent to every consumer and never held, is kept for four anchor periods.
        let mut marks = Vec::new();
        for id in 25..=96u16 {
            if id > 36 {
                marks.extend(
                    feed(&mut w, id..=id)
                        .into_iter()
                        .filter_map(|f| f.1.map(|s| (id, s))),
                );
            }
            assert!(w.acknowledge(id, false));
        }
        assert_eq!(marks, [(84, 0)]);
    }

    #[test]
    fn a_lone_anchor_every_consumer_holds_stays_while_one_lags() {
        // Every consumer is sent each frame and holds each up to 60, then one holds nothing more,
        // as through an outage: the anchor at 48 stays, as one marked over it would be one that
        // consumer lacks, until it is HOLD_MAX old.
        let mut w = ReferenceWindow::with_anchors(8, 1);
        let mut marks = Vec::new();
        for id in 0..=480u16 {
            marks.extend(
                feed(&mut w, id..=id)
                    .into_iter()
                    .filter_map(|f| f.1.map(|s| (id, s))),
            );
            assert!(w.acknowledge(id, id <= 60));
        }
        assert_eq!(
            marks,
            [(0, 0), (48, 0), (432, 0)],
            "past HOLD_MAX it is marked over, the anchor after it kept as one sent is"
        );
    }

    #[test]
    fn a_lone_anchor_is_not_marked_from_the_one_it_marks_over() {
        // Every consumer holds each frame nine frames late, past the seven recent frames: each
        // anchor due would predict from the key frame, so none is marked until that is HOLD_MAX
        // old. Held in time, one is marked every period.
        for (late, want) in [
            (9u16, vec![(0, 0), (384, 0)]),
            (0, vec![(0, 0), (48, 0), (96, 0)]),
        ] {
            let mut w = ReferenceWindow::with_anchors(8, 1);
            let mut marks = Vec::new();
            for id in 0..=(if late > 0 { 480 } else { 100 }) {
                marks.extend(
                    feed(&mut w, id..=id)
                        .into_iter()
                        .filter_map(|f| f.1.map(|s| (id, s))),
                );
                if id >= late {
                    assert!(w.acknowledge(id - late, true));
                }
            }
            assert_eq!(marks, want, "held {late} frames late");
        }
    }

    #[test]
    fn a_consumer_further_behind_than_an_anchor_period_is_predicted_past_on_its_report() {
        let mut w = ReferenceWindow::with_anchors(4, 2);
        for id in 0..=60u16 {
            feed(&mut w, id..=id);
            assert!(w.acknowledge(id, true));
        }
        let anchors: Vec<(u16, Reference)> = feed(&mut w, 61..=120)
            .into_iter()
            .filter(|f| f.1.is_some())
            .map(|f| (f.0, f.2))
            .collect();
        assert_eq!(
            anchors,
            [
                (72, Reference::Frame(71)),
                (84, Reference::Frame(83)),
                (96, Reference::Frame(95)),
                (108, Reference::Frame(107)),
                (120, Reference::Frame(119)),
            ],
            "a consumer holds nothing past 60: of two anchors, each predicts from the frame before"
        );
        assert_eq!(w.invalidate(121), Invalidation::Ignored, "never sent");
        assert!(matches!(w.invalidate(61), Invalidation::Forget(_)));
        assert_eq!(
            w.predicting_anchor(),
            Some(1),
            "60, pinned in slot 1, named"
        );
        assert_eq!(w.record(121, false), Reference::Frame(60));

        let mut w = ReferenceWindow::with_anchors(4, 2);
        feed(&mut w, 0..=120);
        assert_eq!(
            w.invalidate(61),
            Invalidation::KeyFrame,
            "unacknowledged, both anchors are past 61"
        );
    }

    #[test]
    fn a_recent_frame_further_back_than_the_buffer_is_not_predicted_from_after_a_gap() {
        // 48, the anchor, is reported lost with the frames after it, so 47 is the newest frame
        // left. Five frames after the anchor, 47 is seven back, within seven recent, and the
        // next frame predicts from it.
        let mut w = ReferenceWindow::with_anchors(8, 1);
        w.set_frame_num_range(256);
        w.set_acknowledged();
        for id in 0..=47u16 {
            feed(&mut w, id..=id);
            assert!(w.acknowledge(id, true));
        }
        assert_eq!(feed(&mut w, 48..=53)[0].1, Some(0), "48 is the anchor");
        assert_eq!(w.invalidate(48), Invalidation::Forget(48));
        assert_eq!(w.forget_stale(), None);
        assert_eq!(w.record(54, false), Reference::Frame(47));

        // Six frames after it, 47 is eight back past seven recent: a decoder filling the gap has
        // let it go, and with the one anchor lost the next frame is a key frame.
        let mut w = ReferenceWindow::with_anchors(8, 1);
        w.set_frame_num_range(256);
        w.set_acknowledged();
        for id in 0..=47u16 {
            feed(&mut w, id..=id);
            assert!(w.acknowledge(id, true));
        }
        feed(&mut w, 48..=54);
        assert_eq!(w.invalidate(48), Invalidation::Forget(48));
        assert_eq!(w.newest_valid(), Some((47, 47)));
        assert_eq!(w.forget_stale(), Some(Invalidation::KeyFrame));

        let mut w = ReferenceWindow::with_anchors(8, 2);
        w.set_acknowledged();
        feed(&mut w, 0..=12);
        assert_eq!(w.forget_stale(), None, "only H.264 counts frame_num gaps");
    }

    #[test]
    fn the_frame_where_frame_num_wraps_is_an_anchor_once_acknowledged() {
        let mut w = ReferenceWindow::with_anchors(8, 1);
        w.set_frame_num_range(16);
        feed(&mut w, 0..=15);
        assert_eq!(w.plan_anchor(false), None, "16 is off the schedule");
        assert!(w.acknowledge(15, true));
        assert!(w.plan_anchor(false).is_some(), "frame_num wraps at 16");

        // It predicts from the newest frame every consumer holds however far back: the anchor.
        let mut w = ReferenceWindow::with_anchors(8, 1);
        w.set_frame_num_range(64);
        for id in 0..=48u16 {
            feed(&mut w, id..=id);
            assert!(w.acknowledge(id, true));
        }
        let at: Vec<(u16, Reference)> = feed(&mut w, 49..=64)
            .into_iter()
            .filter(|f| f.1.is_some())
            .map(|f| (f.0, f.2))
            .collect();
        assert_eq!(
            at.last(),
            Some(&(64, Reference::Frame(48))),
            "the wrap reaches back to 48, the anchor every consumer holds"
        );

        // A frame every consumer was only sent, not yet held, is no reference for the wrap.
        let mut w = ReferenceWindow::with_anchors(8, 1);
        w.set_frame_num_range(16);
        w.set_acknowledged();
        for id in 0..=15u16 {
            feed(&mut w, id..=id);
            assert!(w.acknowledge(id, id <= 13));
        }
        assert!(w.plan_anchor(false).is_some());
        assert_eq!(w.settle(), Some(Invalidation::Forget(14)));
        assert_eq!(w.record_marked(16, false, Some(0)), Reference::Frame(13));
    }
}
