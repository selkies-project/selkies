/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! Where a region's picture stands, block by block: the per-block counterpart of the cleanup
//! policy's per-region bookkeeping (`pipeline::cleanup_due`).
//!
//! The cleanup policy reads a region as still or moving whole, so a region that never holds
//! still (a video playing in one window) keeps everything else in it at the quality motion is
//! coded at: 1080p text beside a 640x360 video at 8 Mbit/s stayed at 32 dB, where the same
//! text alone was cleaned up to 55. The ledger keeps, for each `BLOCK` x `BLOCK` block, how
//! many frames it has held still and whether its last coding was at the paint-over quality
//! (clean). The capture's damage marks the blocks it covers changed and unclean each frame; a
//! frame that codes blocks finer through the encoder's per-block quantizer
//! (`FrameEncoder::refine_blocks`, x264's quantizer offsets) asks the ledger which
//! (`BlockLedger::plan`): blocks that held still for the cleanup's trigger and are not clean,
//! at most so many a frame, taken in turn from where the last plan stopped so a large set is
//! refined across frames rather than in one, and marks them clean once coded. A whole frame
//! coded at the paint-over quality cleans every block it did not see change; a key frame at the
//! rate control's quality leaves every block unclean. Nothing reads pixels: the damage source
//! did the detection, and a source that over-reports (a client damaging its whole surface)
//! only refines less, never wrongly.

use smithay::utils::{Physical, Rectangle};

/// The side of a block in pixels: an H.264 or VP8 macroblock. An encoder whose quantizer map is
/// coarser (a 32x32 HEVC tree block, a 64x64 AV1 superblock) reads its unit from the blocks it
/// covers (`BlockPlan::refines_area`).
pub const BLOCK: i32 = 16;

/// The blocks of a picture to code at the paint-over quality this frame (`BlockLedger::plan`).
#[derive(Clone, Debug, PartialEq)]
pub struct BlockPlan {
    pub cols: usize,
    pub rows: usize,
    refine: Vec<u64>,
    /// How many blocks the plan refines.
    pub count: usize,
}

impl BlockPlan {
    /// Whether block (`col`, `row`) is refined.
    pub fn refines(&self, col: usize, row: usize) -> bool {
        let i = row * self.cols + col;
        col < self.cols && row < self.rows && self.refine[i / 64] >> (i % 64) & 1 != 0
    }

    /// Whether any refined block lies in the pixel area from (`x`, `y`) of `width` x `height`:
    /// an encoder whose map unit spans several blocks refines a unit that holds one.
    pub fn refines_area(&self, x: i32, y: i32, width: i32, height: i32) -> bool {
        let c0 = (x.max(0) / BLOCK) as usize;
        let r0 = (y.max(0) / BLOCK) as usize;
        let c1 = (((x + width).max(0) + BLOCK - 1) / BLOCK) as usize;
        let r1 = (((y + height).max(0) + BLOCK - 1) / BLOCK) as usize;
        (r0..r1.min(self.rows)).any(|r| (c0..c1.min(self.cols)).any(|c| self.refines(c, r)))
    }
}

/// Per-block stillness and cleanliness of one region (module docblock).
#[derive(Clone, Debug, Default)]
pub struct BlockLedger {
    width: i32,
    height: i32,
    cols: usize,
    rows: usize,
    /// Frames each block has held still, saturating.
    still: Vec<u16>,
    /// Whether each block's last coding was at the paint-over quality or finer.
    clean: Vec<bool>,
    /// The block the next plan starts its turn from.
    next: usize,
}

impl BlockLedger {
    /// A ledger of a `width` x `height` region, every block changed and unclean.
    pub fn new(width: i32, height: i32) -> Self {
        let cols = (width.max(0) + BLOCK - 1) as usize / BLOCK as usize;
        let rows = (height.max(0) + BLOCK - 1) as usize / BLOCK as usize;
        Self {
            width,
            height,
            cols,
            rows,
            still: vec![0; cols * rows],
            clean: vec![false; cols * rows],
            next: 0,
        }
    }

    /// Follow the region to `width` x `height`, starting over where that changed it: a resize,
    /// a scale change, or a rebuilt encoder leaves nothing of the old picture's quality.
    pub fn fit(&mut self, width: i32, height: i32) {
        if (width, height) != (self.width, self.height) {
            *self = Self::new(width, height);
        }
    }

    /// One frame of the region: the blocks `rects` cover changed and are unclean, and every
    /// other block held still one more frame. Damage across frames the capture skipped is the
    /// caller's to union into `rects`.
    pub fn frame<'a>(&mut self, rects: impl IntoIterator<Item = &'a Rectangle<i32, Physical>>) {
        for s in self.still.iter_mut() {
            *s = s.saturating_add(1);
        }
        for r in rects {
            let c0 = (r.loc.x.max(0) / BLOCK) as usize;
            let r0 = (r.loc.y.max(0) / BLOCK) as usize;
            let c1 = ((r.loc.x + r.size.w).clamp(0, self.width) + BLOCK - 1) / BLOCK;
            let r1 = ((r.loc.y + r.size.h).clamp(0, self.height) + BLOCK - 1) / BLOCK;
            for row in r0..(r1 as usize).min(self.rows) {
                for col in c0..(c1 as usize).min(self.cols) {
                    let i = row * self.cols + col;
                    self.still[i] = 0;
                    self.clean[i] = false;
                }
            }
        }
    }

    /// The whole region changed this frame (a source that cannot say where).
    pub fn frame_all(&mut self) {
        self.still.fill(0);
        self.clean.fill(false);
    }

    /// The frame just coded the whole region at the rate control's quality where it was clean
    /// (a key frame a consumer asked for): every block is unclean again.
    pub fn coarsened(&mut self) {
        self.clean.fill(false);
    }

    /// The frame just coded the whole region at the paint-over quality or finer: every block
    /// that did not change in it is clean.
    pub fn cleaned(&mut self) {
        for (clean, &still) in self.clean.iter_mut().zip(&self.still) {
            *clean |= still > 0;
        }
    }

    /// Whether every block that held still is clean: nothing for a cleanup to do.
    pub fn settled(&self) -> bool {
        self.clean
            .iter()
            .zip(&self.still)
            .all(|(&c, &s)| c || s == 0)
    }

    /// The blocks to refine this frame: those still for `trigger` frames or more and not
    /// clean, at most `max`, in turn from where the last plan stopped; `None` where there are
    /// none.
    pub fn plan(&mut self, trigger: u32, max: usize) -> Option<BlockPlan> {
        let n = self.cols * self.rows;
        if n == 0 || max == 0 {
            return None;
        }
        let trigger = trigger.min(u16::MAX as u32) as u16;
        let mut refine = vec![0u64; n.div_ceil(64)];
        let mut count = 0;
        let start = self.next % n;
        for k in 0..n {
            let i = (start + k) % n;
            if self.still[i] >= trigger && !self.clean[i] {
                refine[i / 64] |= 1 << (i % 64);
                count += 1;
                if count == max {
                    self.next = i + 1;
                    break;
                }
            }
        }
        (count > 0).then_some(BlockPlan {
            cols: self.cols,
            rows: self.rows,
            refine,
            count,
        })
    }

    /// The blocks `plan` refined were coded at the paint-over quality: they are clean.
    pub fn refined(&mut self, plan: &BlockPlan) {
        for row in 0..self.rows.min(plan.rows) {
            for col in 0..self.cols.min(plan.cols) {
                if plan.refines(col, row) {
                    self.clean[row * self.cols + col] = true;
                }
            }
        }
    }

    /// The number of blocks in the region.
    pub fn blocks(&self) -> usize {
        self.cols * self.rows
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rect(x: i32, y: i32, w: i32, h: i32) -> Rectangle<i32, Physical> {
        Rectangle::new((x, y).into(), (w, h).into())
    }

    #[test]
    fn blocks_that_held_still_are_refined_while_others_keep_changing() {
        let mut ledger = BlockLedger::new(64, 32);
        let video = rect(32, 0, 32, 32);
        for _ in 0..3 {
            ledger.frame([&video]);
        }
        let plan = ledger.plan(3, usize::MAX).expect("a plan");
        assert_eq!(plan.count, 4, "the two still columns of two rows");
        assert!(plan.refines(0, 0) && plan.refines(1, 1) && !plan.refines(2, 0));
        ledger.refined(&plan);
        ledger.frame([&video]);
        assert_eq!(ledger.plan(3, usize::MAX), None, "nothing left to refine");
        assert!(ledger.settled());
    }

    #[test]
    fn a_large_set_is_refined_in_turn() {
        let mut ledger = BlockLedger::new(64, 64);
        ledger.frame([]);
        let first = ledger.plan(1, 6).unwrap();
        ledger.refined(&first);
        let second = ledger.plan(1, 6).unwrap();
        assert_eq!((first.count, second.count), (6, 6));
        assert!(
            first.refines(0, 0) && second.refines(2, 1),
            "the second takes up where the first stopped"
        );
        ledger.refined(&second);
        let rest = ledger.plan(1, 16).unwrap();
        assert_eq!(rest.count, 4);
    }

    #[test]
    fn a_change_or_a_coarse_key_frame_makes_blocks_unclean_again() {
        let mut ledger = BlockLedger::new(32, 32);
        ledger.frame([]);
        ledger.cleaned();
        assert!(ledger.settled());
        ledger.frame([&rect(0, 0, 1, 1)]);
        assert!(ledger.settled(), "a block that just changed is not due");
        ledger.frame([]);
        assert!(!ledger.settled());
        assert_eq!(ledger.plan(1, 8).unwrap().count, 1);
        ledger.cleaned();
        ledger.coarsened();
        assert_eq!(ledger.plan(1, 8).unwrap().count, 4);
    }

    #[test]
    fn a_coarser_map_unit_refines_where_it_holds_a_refined_block() {
        let mut ledger = BlockLedger::new(64, 64);
        ledger.frame([&rect(0, 0, 64, 48)]);
        let plan = ledger.plan(1, usize::MAX).unwrap();
        assert!(!plan.refines_area(0, 0, 32, 32));
        assert!(
            plan.refines_area(32, 32, 32, 32),
            "the bottom row of blocks"
        );
    }

    #[test]
    fn a_resize_starts_the_ledger_over() {
        let mut ledger = BlockLedger::new(32, 32);
        ledger.frame([]);
        ledger.cleaned();
        ledger.fit(48, 32);
        assert_eq!(ledger.blocks(), 6);
        assert!(ledger.plan(0, 8).is_some(), "every block unclean");
    }
}
