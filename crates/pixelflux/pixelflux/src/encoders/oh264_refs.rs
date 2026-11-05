//! The long-term references of an OpenH264 session, read off its own output.
//!
//! OpenH264 marks a long-term reference on its own schedule, takes it for a recovery only once
//! told the frame was received (`LTR_MARKING_SUCCESS`, naming its `frame_num`), and predicts past
//! a loss from it only in its camera usage: the screen-content strategy ignores the request. Which
//! frame it marked, and which frame each one predicts from, reach the application only in the
//! slice headers, so the session reads its own: `frame_num`, `idr_pic_id`, the list modification
//! that names the reference, and the marking. A marked frame every consumer holds is confirmed;
//! a loss after the confirmed one is predicted past from it (`LTR_RECOVERY_REQUEST`), any other
//! costs a key frame. The frame that marks one predicting from a frame every consumer was sent,
//! and one predicting past a loss from a confirmed one, are anchors (`FRAME_ANCHOR`): each
//! consumer decodes them, and a stream carrying them predicts past a loss of any depth.

use std::collections::VecDeque;

use super::reference::Reference;

/// Frames remembered since the key frame, to map `frame_num` back to frame ids: many marking
/// periods, so a confirmed reference and a loss after it are both still found.
const REMEMBERED: usize = 512;

/// Frames remembered as sent to every consumer, for the marking frames that predict from one.
const SENT: usize = 64;

/// What a loss asks of the encoder.
#[derive(Debug, PartialEq, Eq)]
pub enum Recovery {
    /// The frame predates the key frame, was already predicted past, or was never sent.
    Ignored,
    /// Predict the next frame from the confirmed long-term reference: the `idr_pic_id` and the
    /// `frame_num` of the last frame decoded before the loss, for `SLTRRecoverRequest`.
    Request { idr_pic_id: u32, last_correct: i32 },
    /// No confirmed reference precedes the loss.
    KeyFrame,
}

/// A marking OpenH264 is told of: `(idr_pic_id, frame_num)` of the long-term frame.
pub type Feedback = (u32, i32);

#[derive(Clone, Copy, Default)]
struct Sps {
    log2_max_frame_num: u32,
    poc_type: u32,
    log2_max_poc_lsb: u32,
    delta_always_zero: bool,
    frame_mbs_only: bool,
}

#[derive(Clone, Copy, Default)]
struct Pps {
    sps_id: usize,
    bottom_field_poc: bool,
    redundant_pic_cnt: bool,
}

/// The reference a slice names.
#[derive(Debug, PartialEq, Eq)]
enum Ref {
    /// The default list: the newest short-term frame.
    Newest,
    /// A short-term frame, by `frame_num`.
    Short(u32),
    /// A long-term frame, by long-term index.
    Long(u32),
}

/// One memory management operation of a slice.
#[derive(Debug, PartialEq, Eq)]
enum Mark {
    /// The frame itself becomes long-term at an index (MMCO 6, or an IDR's flag).
    Current(u32),
    /// An earlier short-term frame becomes long-term (MMCO 3), by `frame_num`.
    Earlier(u32, u32),
    /// A long-term index is let go (MMCO 2).
    Unmark(u32),
    /// Indices from this one up are let go (MMCO 4).
    Below(u32),
    /// Every reference is let go (MMCO 5).
    Reset,
}

#[derive(Debug, PartialEq, Eq)]
struct Slice {
    idr: bool,
    frame_num: u32,
    idr_pic_id: u32,
    reference: Ref,
    marks: Vec<Mark>,
}

/// A long-term frame: its index, frame id and `frame_num`, the frame whose header marked it, and
/// whether every consumer was found to hold that one.
#[derive(Clone, Copy, Debug)]
struct LongTerm {
    index: u32,
    id: u16,
    frame_num: u32,
    marked_by: u16,
    confirmed: bool,
}

/// One frame since the key frame: its id, its `frame_num`, and whether a loss had it predicted
/// past.
#[derive(Clone, Copy)]
struct Coded {
    id: u16,
    frame_num: u32,
    forgotten: bool,
}

/// The references of one OpenH264 stream, kept from its own slice headers.
#[derive(Default)]
pub struct LongTermRefs {
    sps: Vec<Option<Sps>>,
    pps: Vec<Option<Pps>>,
    idr_pic_id: u32,
    frames: VecDeque<Coded>,
    long_term: Vec<LongTerm>,
    /// The index confirmed but not yet acted on: OpenH264 drops its other long-term frames once
    /// the next predicted frame is coded.
    confirming: Option<u32>,
    /// The newest frames every consumer was sent.
    sent: VecDeque<u16>,
}

impl LongTermRefs {
    pub fn new() -> Self {
        Self::default()
    }

    /// Read one access unit the encoder emitted for frame `id`: the frame it predicts from, and
    /// whether it is an anchor (`FRAME_ANCHOR`).
    pub fn record(&mut self, id: u16, au: &[u8]) -> (Reference, bool) {
        let mut slice = None;
        for nal in super::codec::annexb_nals(au) {
            let Some(&head) = nal.first() else { continue };
            let rbsp = unescape(&nal[1..]);
            match head & 0x1f {
                7 => self.read_sps(&rbsp),
                8 => self.read_pps(&rbsp),
                1 | 5 if slice.is_none() => slice = self.read_slice(&rbsp, head),
                _ => {}
            }
        }
        let Some(slice) = slice else {
            return (Reference::Untracked, false);
        };
        if slice.idr {
            self.idr_pic_id = slice.idr_pic_id;
            self.frames.clear();
            self.long_term.clear();
            self.confirming = None;
            self.sent.clear();
            // OpenH264 takes its key frame as received.
            if slice.marks.contains(&Mark::Current(0)) {
                self.long_term.push(LongTerm {
                    index: 0,
                    id,
                    frame_num: slice.frame_num,
                    marked_by: id,
                    confirmed: true,
                });
            }
            self.push(id, slice.frame_num);
            return (Reference::None, false);
        }
        let (reference, mut anchor) = match slice.reference {
            // An anchor predicts past frames from one every consumer holds; the frame after a
            // long-term one predicts from it as from any newest frame.
            Ref::Long(index) => match self.long_term.iter().find(|l| l.index == index) {
                Some(l) => (
                    Reference::Frame(l.id),
                    l.confirmed && self.frames.back().is_some_and(|f| f.id != l.id),
                ),
                None => (Reference::Untracked, false),
            },
            Ref::Short(frame_num) => {
                match self.frames.iter().rev().find(|f| f.frame_num == frame_num) {
                    Some(f) => (Reference::Frame(f.id), false),
                    None => (Reference::Untracked, false),
                }
            }
            Ref::Newest => match self.frames.back() {
                Some(f) => (Reference::Frame(f.id), false),
                None => (Reference::Untracked, false),
            },
        };
        if let Some(index) = self.confirming.take() {
            self.long_term.retain(|l| l.index == index);
        }
        let marks = slice
            .marks
            .iter()
            .any(|m| matches!(m, Mark::Current(_) | Mark::Earlier(..)));
        anchor |= marks && matches!(reference, Reference::Frame(r) if self.sent.contains(&r));
        for mark in &slice.marks {
            match *mark {
                Mark::Current(index) => self.mark(index, id, slice.frame_num, id),
                Mark::Earlier(frame_num, index) => {
                    if let Some(f) = self.frames.iter().rev().find(|f| f.frame_num == frame_num) {
                        let earlier = f.id;
                        self.mark(index, earlier, frame_num, id);
                    }
                }
                Mark::Unmark(index) => self.long_term.retain(|l| l.index != index),
                Mark::Below(max) => self.long_term.retain(|l| l.index < max),
                Mark::Reset => self.long_term.clear(),
            }
        }
        self.push(id, slice.frame_num);
        (reference, anchor)
    }

    fn mark(&mut self, index: u32, id: u16, frame_num: u32, marked_by: u16) {
        self.long_term.retain(|l| l.index != index);
        self.long_term.push(LongTerm {
            index,
            id,
            frame_num,
            marked_by,
            confirmed: false,
        });
    }

    fn push(&mut self, id: u16, frame_num: u32) {
        self.frames.push_back(Coded {
            id,
            frame_num,
            forgotten: false,
        });
        while self.frames.len() > REMEMBERED {
            self.frames.pop_front();
        }
    }

    /// Every consumer holds frame `id`, or where not `held` was sent it: where a held one is the
    /// frame that marked a long-term one, the marking to confirm to the encoder.
    pub fn acknowledge(&mut self, id: u16, held: bool) -> Option<Feedback> {
        if !self.sent.contains(&id) {
            self.sent.push_back(id);
            if self.sent.len() > SENT {
                self.sent.pop_front();
            }
        }
        if !held {
            return None;
        }
        let l = self
            .long_term
            .iter_mut()
            .find(|l| !l.confirmed && l.marked_by == id)?;
        l.confirmed = true;
        self.confirming = Some(l.index);
        Some((self.idr_pic_id, l.frame_num as i32))
    }

    /// A consumer lost frame `id`: what the encoder is asked, and the markings to report failed
    /// (a long-term frame the loss reaches, which OpenH264 otherwise waits on).
    pub fn invalidate(&mut self, id: u16) -> (Recovery, Vec<Feedback>) {
        let Some(at) = self.frames.iter().position(|f| f.id == id) else {
            let newest = self.frames.back().map(|f| f.id);
            let ahead = newest.is_some_and(|n| id.wrapping_sub(n) < 0x8000);
            let whole = self.frames.front().is_some_and(|f| f.frame_num == 0);
            return if ahead || whole {
                (Recovery::Ignored, Vec::new())
            } else {
                (Recovery::KeyFrame, Vec::new())
            };
        };
        if self.frames[at].forgotten {
            return (Recovery::Ignored, Vec::new());
        }
        let after = |id: u16, frames: &VecDeque<Coded>| {
            frames
                .iter()
                .position(|f| f.id == id)
                .is_none_or(|p| p >= at)
        };
        let mut failed = Vec::new();
        let idr_pic_id = self.idr_pic_id;
        self.long_term.retain(|l| {
            let reached =
                !l.confirmed && (after(l.id, &self.frames) || after(l.marked_by, &self.frames));
            if reached {
                failed.push((idr_pic_id, l.frame_num as i32));
            }
            !reached
        });
        // FFmpeg's decoder, which most browsers decode with, misorders the pictures past a
        // `frame_num` wrap it did not see.
        let wraps = self.frames.iter().skip(at).any(|f| f.frame_num == 0);
        let confirmed: Vec<usize> = self
            .long_term
            .iter()
            .filter(|l| l.confirmed)
            .filter_map(|l| self.frames.iter().position(|f| f.id == l.id))
            .collect();
        if at == 0 || wraps || confirmed.is_empty() || confirmed.iter().any(|&p| p >= at) {
            return (Recovery::KeyFrame, failed);
        }
        for f in self.frames.iter_mut().skip(at) {
            f.forgotten = true;
        }
        let last_correct = self.frames[at - 1].frame_num as i32;
        (
            Recovery::Request {
                idr_pic_id,
                last_correct,
            },
            failed,
        )
    }

    fn read_sps(&mut self, rbsp: &[u8]) {
        let mut r = Bits::new(rbsp);
        let Some(sps) = (|| {
            let profile = r.bits(8)?;
            r.bits(16)?;
            let id = r.ue()? as usize;
            if matches!(
                profile,
                100 | 110 | 122 | 244 | 44 | 83 | 86 | 118 | 128 | 138 | 139 | 134 | 135
            ) {
                // OpenH264 codes Constrained Baseline; a high profile set is not one of its own.
                return None;
            }
            let log2_max_frame_num = r.ue()? + 4;
            let poc_type = r.ue()?;
            let (mut log2_max_poc_lsb, mut delta_always_zero) = (0, false);
            match poc_type {
                0 => log2_max_poc_lsb = r.ue()? + 4,
                1 => {
                    delta_always_zero = r.bit()?;
                    r.se()?;
                    r.se()?;
                    for _ in 0..r.ue()? {
                        r.se()?;
                    }
                }
                _ => {}
            }
            r.ue()?;
            r.bit()?;
            r.ue()?;
            r.ue()?;
            let frame_mbs_only = r.bit()?;
            Some((
                id,
                Sps {
                    log2_max_frame_num,
                    poc_type,
                    log2_max_poc_lsb,
                    delta_always_zero,
                    frame_mbs_only,
                },
            ))
        })() else {
            return;
        };
        if self.sps.len() <= sps.0 {
            self.sps.resize(sps.0 + 1, None);
        }
        self.sps[sps.0] = Some(sps.1);
    }

    fn read_pps(&mut self, rbsp: &[u8]) {
        let mut r = Bits::new(rbsp);
        let Some(pps) = (|| {
            let id = r.ue()? as usize;
            let sps_id = r.ue()? as usize;
            r.bit()?;
            let bottom_field_poc = r.bit()?;
            if r.ue()? > 0 {
                // Slice groups: not a stream OpenH264 codes.
                return None;
            }
            r.ue()?;
            r.ue()?;
            r.bit()?;
            r.bits(2)?;
            r.se()?;
            r.se()?;
            r.se()?;
            r.bit()?;
            r.bit()?;
            let redundant_pic_cnt = r.bit()?;
            Some((
                id,
                Pps {
                    sps_id,
                    bottom_field_poc,
                    redundant_pic_cnt,
                },
            ))
        })() else {
            return;
        };
        if self.pps.len() <= pps.0 {
            self.pps.resize(pps.0 + 1, None);
        }
        self.pps[pps.0] = Some(pps.1);
    }

    /// The header of a slice of a Constrained Baseline stream, through `dec_ref_pic_marking`.
    fn read_slice(&self, rbsp: &[u8], head: u8) -> Option<Slice> {
        let idr = head & 0x1f == 5;
        let reference_idc = (head >> 5) & 3;
        let mut r = Bits::new(rbsp);
        r.ue()?;
        let slice_type = r.ue()? % 5;
        let pps = (*self.pps.get(r.ue()? as usize)?)?;
        let sps = (*self.sps.get(pps.sps_id)?)?;
        let frame_num = r.bits(sps.log2_max_frame_num)?;
        if !sps.frame_mbs_only && r.bit()? {
            // Field pictures: not a stream OpenH264 codes.
            return None;
        }
        let idr_pic_id = if idr { r.ue()? } else { 0 };
        if sps.poc_type == 0 {
            r.bits(sps.log2_max_poc_lsb)?;
            if pps.bottom_field_poc {
                r.se()?;
            }
        } else if sps.poc_type == 1 && !sps.delta_always_zero {
            r.se()?;
            if pps.bottom_field_poc {
                r.se()?;
            }
        }
        if pps.redundant_pic_cnt {
            r.ue()?;
        }
        let mut reference = Ref::Newest;
        if slice_type == 0 || slice_type == 3 {
            if r.bit()? {
                r.ue()?;
            }
            if r.bit()? {
                let max = 1u32 << sps.log2_max_frame_num;
                let mut predicted = frame_num;
                let mut first = true;
                loop {
                    let idc = r.ue()?;
                    let found = match idc {
                        0 | 1 => {
                            let diff = r.ue()? + 1;
                            predicted = if idc == 0 {
                                (predicted + max - diff % max) % max
                            } else {
                                (predicted + diff) % max
                            };
                            Ref::Short(predicted)
                        }
                        2 => Ref::Long(r.ue()?),
                        _ => break,
                    };
                    if first {
                        reference = found;
                        first = false;
                    }
                }
            }
        }
        let mut marks = Vec::new();
        if reference_idc != 0 {
            if idr {
                r.bit()?;
                if r.bit()? {
                    marks.push(Mark::Current(0));
                }
            } else if r.bit()? {
                let max = 1u32 << sps.log2_max_frame_num;
                loop {
                    match r.ue()? {
                        0 => break,
                        1 => {
                            r.ue()?;
                        }
                        2 => marks.push(Mark::Unmark(r.ue()?)),
                        3 => {
                            let diff = r.ue()? + 1;
                            let index = r.ue()?;
                            marks.push(Mark::Earlier((frame_num + max - diff % max) % max, index));
                        }
                        4 => marks.push(Mark::Below(r.ue()?)),
                        5 => marks.push(Mark::Reset),
                        6 => marks.push(Mark::Current(r.ue()?)),
                        _ => return None,
                    }
                }
            }
        }
        Some(Slice {
            idr,
            frame_num,
            idr_pic_id,
            reference,
            marks,
        })
    }
}

fn unescape(nal: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(nal.len());
    let mut zeros = 0;
    for &b in nal {
        if zeros >= 2 && b == 3 {
            zeros = 0;
            continue;
        }
        zeros = if b == 0 { zeros + 1 } else { 0 };
        out.push(b);
    }
    out
}

struct Bits<'a> {
    rbsp: &'a [u8],
    pos: usize,
}

impl<'a> Bits<'a> {
    fn new(rbsp: &'a [u8]) -> Self {
        Self { rbsp, pos: 0 }
    }

    fn bit(&mut self) -> Option<bool> {
        let byte = self.rbsp.get(self.pos / 8)?;
        let bit = (byte >> (7 - self.pos % 8)) & 1;
        self.pos += 1;
        Some(bit == 1)
    }

    fn bits(&mut self, n: u32) -> Option<u32> {
        let mut v = 0;
        for _ in 0..n {
            v = (v << 1) | u32::from(self.bit()?);
        }
        Some(v)
    }

    fn ue(&mut self) -> Option<u32> {
        let mut zeros = 0;
        while !self.bit()? {
            zeros += 1;
            if zeros > 31 {
                return None;
            }
        }
        Some((1u32 << zeros) - 1 + self.bits(zeros)?)
    }

    fn se(&mut self) -> Option<i32> {
        let k = self.ue()? as i64;
        Some(if k % 2 == 1 { (k + 1) / 2 } else { -(k / 2) } as i32)
    }
}
