/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! Copy hints: the macroblocks of a frame whose picture is the previous frame's moved by a
//! whole-pixel vector, as when a page scrolls, for an encoder that codes such a macroblock as a
//! copy of its own reconstruction at that vector instead of searching for it (x264's
//! `X264_MBINFO_COPY`).
//!
//! A hint is a candidate. The encoder compares the macroblock with the previous input picture at
//! the vector before it codes a copy, so a wrong hint costs that compare and never the picture,
//! and the detector may be cheap: [`ScrollDetector`] hashes the luma rows of a few narrow strips
//! across the frame and proposes, for the macroblocks around a strip, the shift that maps most of
//! its changed rows onto rows of the previous frame, which is how a page scrolls in an application
//! that redraws it rather than copying it.

/// Width in samples of each strip the detector hashes a row of.
const STRIP: usize = 32;
/// Rows of a strip that must agree on one shift before it is a candidate: a few lines of text.
const MIN_VOTES: u32 = 8;
/// Farthest shift the detector looks for, in rows: H.264's vertical vector range from level 3.1.
const MAX_SHIFT: i32 = 512;
/// How far, in pixels, a macroblock may lie from a strip and still take that strip's shift: the
/// strips are spaced so that no macroblock lies farther from one.
const STRIP_REACH: i32 = 256;

/// A row's hash and whether its samples are all one value, which matches every other such row
/// and so says nothing of a shift.
#[derive(Clone, Copy, Default, PartialEq, Eq)]
struct RowHash {
    hash: u64,
    flat: bool,
}

fn row_hash(bytes: &[u8]) -> RowHash {
    let first = u64::from_le_bytes([bytes[0]; 8]);
    let mut hash = 0x9e37_79b9_7f4a_7c15u64;
    let mut flat = true;
    for chunk in bytes.as_chunks::<8>().0 {
        let v = u64::from_le_bytes(*chunk);
        flat &= v == first;
        hash = (hash ^ v).wrapping_mul(0xff51_afd7_ed55_8ccd);
        hash ^= hash >> 32;
    }
    RowHash { hash, flat }
}

/// Finds vertical scrolls from the picture: the luma rows of a few narrow strips across the
/// frame are hashed every frame, and a strip whose changed rows mostly match the previous frame's
/// rows a fixed number of rows away proposes that shift for the macroblocks around it. A frame
/// where little changed hashes only the rows that did.
#[derive(Default)]
pub struct ScrollDetector {
    width: usize,
    height: usize,
    /// The left edge of each strip.
    strips: Vec<usize>,
    /// The previous frame's row hashes, strip after strip.
    prev: Vec<RowHash>,
    cur: Vec<RowHash>,
    /// Each strip's shift for this frame, where its rows agree on one.
    shifts: Vec<Option<i32>>,
    /// The vote's table of the previous frame's row hashes and its tally, kept across frames.
    table: Vec<(u64, u32, bool)>,
    votes: Vec<u32>,
}

impl ScrollDetector {
    /// Take the frame's 8-bit luma plane (`luma`, `stride` bytes a row) and the macroblocks that
    /// changed since the previous frame (`changed`, in raster order over the whole 16x16
    /// macroblocks, `None` where the source cannot say), and find each strip's shift. The
    /// previous frame is the one the last call took, so the detector runs once for every frame
    /// the encoder codes.
    pub fn detect(
        &mut self,
        luma: &[u8],
        stride: usize,
        width: usize,
        height: usize,
        changed: Option<&[bool]>,
    ) {
        let fresh = width != self.width || height != self.height;
        if fresh {
            let n = width.div_ceil(2 * STRIP_REACH as usize).clamp(2, 16);
            self.width = width;
            self.height = height;
            self.strips = if width < STRIP {
                Vec::new()
            } else {
                (0..n)
                    .map(|i| ((width * (2 * i + 1) / (2 * n)) & !15).min(width - STRIP))
                    .collect()
            };
            self.prev = vec![RowHash::default(); self.strips.len() * height];
        }
        self.cur.clear();
        self.cur.extend_from_slice(&self.prev);
        self.shifts.clear();
        self.shifts.resize(self.strips.len(), None);
        let mb_w = width.div_ceil(16);
        let row_changed = |x: usize, row: usize| {
            fresh
                || changed
                    .is_none_or(|c| c.get((row / 16) * mb_w + x / 16).copied().unwrap_or(true))
        };
        for (s, &x) in self.strips.iter().enumerate() {
            let mut moved = 0usize;
            for row in 0..height {
                if row_changed(x, row) {
                    let start = row * stride + x;
                    self.cur[s * height + row] = row_hash(&luma[start..start + STRIP]);
                    moved += 1;
                }
            }
            if !fresh && moved >= MIN_VOTES as usize {
                self.shifts[s] = Self::vote(
                    &mut self.table,
                    &mut self.votes,
                    &self.prev[s * height..(s + 1) * height],
                    &self.cur[s * height..(s + 1) * height],
                    |row| row_changed(x, row),
                );
            }
        }
        std::mem::swap(&mut self.prev, &mut self.cur);
    }

    /// The shift that maps most of a strip's changed rows onto rows of the previous frame whose
    /// hash is unique there, where at least `MIN_VOTES` agree.
    fn vote(
        table: &mut Vec<(u64, u32, bool)>,
        votes: &mut Vec<u32>,
        prev: &[RowHash],
        cur: &[RowHash],
        changed: impl Fn(usize) -> bool,
    ) -> Option<i32> {
        let bits = (2 * prev.len()).next_power_of_two().trailing_zeros().max(1);
        let mask = (1usize << bits) - 1;
        let slot = |hash: u64| (hash >> (64 - bits)) as usize;
        table.clear();
        table.resize(mask + 1, (0u64, u32::MAX, false));
        for (row, h) in prev.iter().enumerate().filter(|(_, h)| !h.flat) {
            let mut i = slot(h.hash);
            loop {
                let e = &mut table[i];
                if e.1 == u32::MAX {
                    *e = (h.hash, row as u32, true);
                    break;
                }
                if e.0 == h.hash {
                    e.2 = false;
                    break;
                }
                i = (i + 1) & mask;
            }
        }
        let find = |hash: u64| {
            let mut i = slot(hash);
            loop {
                let e = table[i];
                if e.1 == u32::MAX {
                    return None;
                }
                if e.0 == hash {
                    return e.2.then_some(e.1);
                }
                i = (i + 1) & mask;
            }
        };
        votes.clear();
        votes.resize(2 * MAX_SHIFT as usize + 1, 0);
        for (row, h) in cur.iter().enumerate() {
            if h.flat || !changed(row) {
                continue;
            }
            if let Some(from) = find(h.hash) {
                let shift = from as i32 - row as i32;
                if shift != 0 && shift.abs() <= MAX_SHIFT {
                    votes[(shift + MAX_SHIFT) as usize] += 1;
                }
            }
        }
        votes
            .iter()
            .enumerate()
            .filter(|&(_, &n)| n >= MIN_VOTES)
            .max_by_key(|&(i, &n)| (n, -(i as i32 - MAX_SHIFT).abs()))
            .map(|(i, _)| i as i32 - MAX_SHIFT)
    }

    /// The shift proposed for a macroblock whose left edge is at `x`: the nearest strip's within
    /// `STRIP_REACH`, where it found one.
    fn shift_at(&self, x: usize) -> Option<i32> {
        self.strips
            .iter()
            .zip(&self.shifts)
            .filter(|&(&s, _)| (s as i32 - x as i32).abs() <= STRIP_REACH)
            .min_by_key(|&(&s, _)| (s as i32 - x as i32).abs())
            .and_then(|(_, &shift)| shift)
    }
}

/// Each macroblock's copy hint, in raster order over the whole 16x16 macroblocks of a `width` x
/// `height` frame: for a changed one (`changed`, `None` where every one may have), the scroll
/// `detector` found around it, where the block it names lies inside the frame. A macroblock the
/// frame's edge cuts takes none. Returns how many took one.
pub fn macroblock_vectors(
    width: usize,
    height: usize,
    changed: Option<&[bool]>,
    detector: &ScrollDetector,
    out: &mut Vec<Option<(i16, i16)>>,
) -> usize {
    let (mb_w, mb_h) = (width.div_ceil(16), height.div_ceil(16));
    out.clear();
    out.resize(mb_w * mb_h, None);
    let mut hinted = 0;
    let inside =
        |x: i32, y: i32| x >= 0 && y >= 0 && x + 16 <= width as i32 && y + 16 <= height as i32;
    let columns: Vec<Option<i16>> = (0..mb_w)
        .map(|bx| {
            detector
                .shift_at(bx * 16)
                .and_then(|dy| i16::try_from(dy).ok())
                .filter(|&dy| inside((bx * 16) as i32, dy as i32))
        })
        .collect();
    if columns.iter().all(Option::is_none) {
        return 0;
    }
    for by in 0..mb_h {
        for (bx, &column) in columns.iter().enumerate() {
            let i = by * mb_w + bx;
            let (x, y) = ((bx * 16) as i32, (by * 16) as i32);
            if let Some(dy) = column
                && inside(x, y)
                && inside(x, y.saturating_add(dy as i32))
                && changed.is_none_or(|c| c.get(i).copied().unwrap_or(true))
            {
                out[i] = Some((0, dy));
                hinted += 1;
            }
        }
    }
    hinted
}

/// A luma plane of rows that differ from one another, drawn from `seed` row by row.
#[cfg(test)]
pub(crate) fn page(width: usize, height: usize, seed: u64) -> Vec<u8> {
    let mut out = vec![0u8; width * height];
    for row in 0..height {
        let mut s = (row as u64 + seed).wrapping_mul(0x9e37_79b9_7f4a_7c15) | 1;
        for px in &mut out[row * width..(row + 1) * width] {
            s ^= s << 13;
            s ^= s >> 7;
            s ^= s << 17;
            *px = s as u8;
        }
    }
    out
}

/// `rows` of `page` from `top`, a window onto it scrolled to `top`.
#[cfg(test)]
pub(crate) fn window(page: &[u8], width: usize, top: usize, rows: usize) -> Vec<u8> {
    page[top * width..(top + rows) * width].to_vec()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_scrolled_page_proposes_its_shift() {
        let (w, h) = (1024, 512);
        let doc = page(w, 4 * h, 7);
        let mut d = ScrollDetector::default();
        d.detect(&window(&doc, w, 0, h), w, w, h, None);
        d.detect(&window(&doc, w, 24, h), w, w, h, None);
        assert!(d.shifts.iter().all(|&s| s == Some(24)), "{:?}", d.shifts);
        let mut out = Vec::new();
        let n = macroblock_vectors(w, h, None, &d, &mut out);
        let mb_w = w / 16;
        assert_eq!(out[0], Some((0, 24)));
        assert_eq!(
            out[(h / 16 - 1) * mb_w],
            None,
            "a block whose source lies past the bottom edge takes no hint"
        );
        assert_eq!(n, (h - 32) / 16 * mb_w);
    }

    #[test]
    fn a_page_scrolled_back_proposes_a_negative_shift() {
        let (w, h) = (512, 256);
        let doc = page(w, 4 * h, 3);
        let mut d = ScrollDetector::default();
        d.detect(&window(&doc, w, 100, h), w, w, h, None);
        d.detect(&window(&doc, w, 60, h), w, w, h, None);
        assert!(d.shifts.iter().all(|&s| s == Some(-40)), "{:?}", d.shifts);
    }

    #[test]
    fn flat_rows_propose_nothing() {
        let (w, h) = (512, 256);
        let mut d = ScrollDetector::default();
        let white = vec![235u8; w * h];
        d.detect(&white, w, w, h, None);
        d.detect(&white, w, w, h, None);
        assert!(d.shifts.iter().all(|s| s.is_none()));
    }

    #[test]
    fn a_frame_of_new_rows_proposes_nothing() {
        let (w, h) = (512, 256);
        let mut d = ScrollDetector::default();
        d.detect(&page(w, h, 1), w, w, h, None);
        d.detect(&page(w, h, 9_000), w, w, h, None);
        assert!(d.shifts.iter().all(|s| s.is_none()));
    }

    #[test]
    fn a_resize_starts_over() {
        let (w, h) = (512, 256);
        let doc = page(w, 4 * h, 5);
        let mut d = ScrollDetector::default();
        d.detect(&window(&doc, w, 0, h), w, w, h, None);
        d.detect(&window(&doc, w, 16, h - 16), w, w, h - 16, None);
        assert!(d.shifts.iter().all(|s| s.is_none()));
    }

    #[test]
    fn only_changed_rows_are_hashed_again() {
        let (w, h) = (512, 256);
        let doc = page(w, 4 * h, 11);
        let mut d = ScrollDetector::default();
        let first = window(&doc, w, 0, h);
        d.detect(&first, w, w, h, None);
        let mut second = window(&doc, w, 32, h);
        let unchanged = vec![false; (w / 16) * (h / 16)];
        d.detect(&second, w, w, h, Some(&unchanged));
        assert!(
            d.shifts.iter().all(|s| s.is_none()),
            "nothing changed, nothing hashed"
        );
        second.copy_from_slice(&first);
        d.detect(&second, w, w, h, None);
        assert!(d.shifts.iter().all(|s| s.is_none()));
    }

    #[test]
    fn an_unchanged_block_takes_no_hint() {
        let (w, h) = (256, 128);
        let doc = page(w, 4 * h, 13);
        let mut d = ScrollDetector::default();
        d.detect(&window(&doc, w, 0, h), w, w, h, None);
        d.detect(&window(&doc, w, 16, h), w, w, h, None);
        let mut changed = vec![true; (w / 16) * (h / 16)];
        changed[0] = false;
        let mut out = Vec::new();
        macroblock_vectors(w, h, Some(&changed), &d, &mut out);
        assert_eq!(out[0], None);
        assert_eq!(out[1], Some((0, 16)));
    }
}

#[cfg(test)]
mod bench {
    use super::*;
    use std::time::Instant;

    /// The detector's cost on a 2560x1600 frame: a page scrolled by 24 rows, and a window of a
    /// quarter of the screen moved diagonally (no vertical scroll to find). Prints medians.
    #[test]
    #[ignore]
    fn detector_cost() {
        let (w, h) = (2560usize, 1600usize);
        let doc = page(w, 2 * h, 5);
        let mb_w = w / 16;
        let mut d = ScrollDetector::default();
        let mut out = Vec::new();
        let mut t_scroll = Vec::new();
        let mut t_drag = Vec::new();
        for k in 0..40 {
            let frame = window(&doc, w, 24 * (k % 20), h);
            let t = Instant::now();
            d.detect(&frame, w, w, h, None);
            macroblock_vectors(w, h, None, &d, &mut out);
            t_scroll.push(t.elapsed().as_secs_f64() * 1e3);
        }
        let mut d = ScrollDetector::default();
        for k in 0..40 {
            let mut frame = window(&doc, w, 0, h);
            let (x0, y0) = (200 + 9 * (k % 20), 150 + 5 * (k % 20));
            for y in y0..y0 + h / 2 {
                for x in x0..x0 + w / 2 {
                    frame[y * w + x] = doc[(y - y0 + h) * w + x - x0];
                }
            }
            let changed: Vec<bool> = (0..mb_w * (h / 16))
                .map(|i| {
                    let (bx, by) = ((i % mb_w) * 16, (i / mb_w) * 16);
                    bx + 16 > x0 - 16
                        && bx < x0 + w / 2 + 16
                        && by + 16 > y0 - 16
                        && by < y0 + h / 2 + 16
                })
                .collect();
            let t = Instant::now();
            d.detect(&frame, w, w, h, Some(&changed));
            macroblock_vectors(w, h, Some(&changed), &d, &mut out);
            t_drag.push(t.elapsed().as_secs_f64() * 1e3);
        }
        t_scroll.sort_by(f64::total_cmp);
        t_drag.sort_by(f64::total_cmp);
        println!(
            "detector: scroll {:.3} ms, drag {:.3} ms",
            t_scroll[20], t_drag[20]
        );
    }
}
