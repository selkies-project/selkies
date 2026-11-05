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

/// Quality index steps finer than the paint-over one x264 codes a refined block at, so the block
/// comes to what the region's own cleanup reaches: one predicted frame at the paint-over
/// quantizer left 1080p text at 44.6 dB, where the cleanup's key frame, at x264's intra offset,
/// reached 49.1; six steps finer reached 49.4. Each encoder that refines says its own
/// (`FrameEncoder::refines`).
pub const REFINE_FINER: u32 = 6;

/// The most blocks a region refines a frame, as a share of it: a large set is refined over
/// `REFINE_FRAMES` frames at least rather than in one frame the size of a cleanup. A constant
/// quality refines that many; a constant rate starts at a share of `REFINE_START_FRAMES` and
/// follows its rate control (`RefinePace`).
const REFINE_FRAMES: usize = 64;
const REFINE_START_FRAMES: usize = 512;

/// Quality index steps the rate control's frames may come out coarser than before the region
/// started refining before its refined blocks a frame halve (or past `REFINE_OVER`); where they
/// come out no coarser, a quarter more. Dense 1080p text refined at 128 blocks a frame beside a
/// video at 8 Mbit/s ran x264's row-level control past quantizer 51, where it scales every
/// macroblock offset toward nothing, and left the text at 42 dB; at 16 a frame it reached 48.
const REFINE_RISE: i32 = 2;

/// Frame budgets, or as much as the frames before refining came to where that is more, a
/// refining frame may come out at before its refined blocks a frame halve, as they do past
/// `REFINE_RISE`: a rate control pinned at its coarsest quantizer says nothing through it, and
/// at 2 Mbit/s at 1080p x264 sent the refinement at twice the rate.
const REFINE_OVER: f64 = 1.5;

/// How an encoder refines blocks (`FrameEncoder::refines`): the quality index steps finer than
/// the paint-over one it codes them at, so a refined block comes to what the region's own
/// cleanup reaches (`finer`); the most steps off the frame's own quality one frame may code a
/// block at (`reach`), past which its rate control loses the rate; and the most runs of blocks
/// along a row one frame may refine (`runs`), where the encoder takes regions rather than a map.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Refiner {
    pub finer: u32,
    pub reach: u32,
    pub runs: usize,
}

impl Refiner {
    /// A refiner `finer` steps past the paint-over quality, with no bound on its reach or runs.
    pub const fn finer(finer: u32) -> Self {
        Self {
            finer,
            reach: u32::MAX,
            runs: usize::MAX,
        }
    }
}

/// A frame's refinement: the blocks (`plan`), the quality index they are coded at (`target`),
/// the rate control's index the offset to it was taken from (`from`), and the most steps the
/// offset may take (`reach`, `Refiner::reach`).
#[derive(Clone, Debug, PartialEq)]
pub struct Refinement {
    pub plan: BlockPlan,
    pub target: i32,
    pub from: i32,
    pub reach: u32,
}

impl Refinement {
    /// The quality index steps the refined blocks are coded off the frame's own.
    pub fn offset(&self) -> i32 {
        (self.target - self.from).clamp(-(self.reach.min(i32::MAX as u32) as i32), 0)
    }
}

/// Frames a region refines nothing for once a frame refining one block came out past
/// `REFINE_OVER` or `REFINE_RISE`: a rate that cannot afford a block a frame is one where the
/// encoder's own refinement of the region does better than a block a frame. VP8 at 2 Mbit/s
/// beside a video came to 24.2 dB refining a block a frame, against 29.6 with none, its cyclic
/// refresh, which the region map displaces, refining the still screen itself.
const REFINE_PAUSE: u32 = 64;

/// The frame budgets the frames of a region refining nothing may average before it refines
/// nothing for `REFINE_PAUSE` frames: a rate control that already runs over its rate has none to
/// spare. Beside a video, VP8's frames averaged 1.38 budgets at 2 Mbit/s, where refining took
/// the text to 25.5 dB against 29.6 by its cyclic refresh and 12% more bytes, and 0.76 at 8
/// Mbit/s, where it took the text from 33.5 to 42.6.
const REFINE_RATE_HELD: f64 = 1.2;

/// How many blocks a region refines a frame (`REFINE_FRAMES`, `REFINE_RISE`, `REFINE_OVER`,
/// `REFINE_PAUSE`, `REFINE_RATE_HELD`).
#[derive(Clone, Debug, Default)]
pub struct RefinePace {
    count: usize,
    base: Option<i32>,
    /// The bytes of the frames that refined nothing, on average over the last eight or so.
    base_bytes: f64,
    paused: u32,
}

impl RefinePace {
    /// The most blocks of a region of `blocks` this frame refines, under a constant rate or not.
    pub fn most(&mut self, blocks: usize, constant_rate: bool) -> usize {
        let most = blocks.div_ceil(REFINE_FRAMES);
        if !constant_rate {
            return most;
        }
        if self.paused > 0 {
            self.paused -= 1;
            return 0;
        }
        if self.count == 0 {
            self.count = blocks.div_ceil(REFINE_START_FRAMES);
        }
        self.count.min(most)
    }

    /// A frame the rate control coded at quality index `coded` in `bytes` against a `budget`,
    /// which refined blocks or not, of a region of `blocks`.
    pub fn coded(&mut self, refined: bool, coded: i32, bytes: usize, budget: f64, blocks: usize) {
        if !refined {
            self.base = Some(coded);
            self.base_bytes += (bytes as f64 - self.base_bytes) / 8.0;
            if self.base_bytes > budget * REFINE_RATE_HELD {
                self.paused = REFINE_PAUSE;
            }
            return;
        }
        let base = *self.base.get_or_insert(coded);
        let budget = budget.max(self.base_bytes);
        let most = blocks.div_ceil(REFINE_FRAMES);
        let count = self.count;
        let costly = bytes as f64 > budget * REFINE_OVER || coded > base + REFINE_RISE;
        if costly && count <= 1 {
            self.paused = REFINE_PAUSE;
        }
        self.count = if costly {
            (count / 2).max(1)
        } else if coded <= base {
            (count + count / 4 + 1).min(most)
        } else {
            count
        };
    }
}

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
    /// clean, at most `max` in at most `runs` runs along a row, in turn from where the last
    /// plan stopped; `None` where there are none.
    pub fn plan(&mut self, trigger: u32, max: usize, runs: usize) -> Option<BlockPlan> {
        let n = self.cols * self.rows;
        if n == 0 || max == 0 {
            return None;
        }
        let trigger = trigger.min(u16::MAX as u32) as u16;
        let mut refine = vec![0u64; n.div_ceil(64)];
        let (mut count, mut started) = (0, 0);
        let mut last: Option<usize> = None;
        let start = self.next % n;
        for k in 0..n {
            let i = (start + k) % n;
            if self.still[i] >= trigger && !self.clean[i] {
                let continues = last.is_some_and(|l| l + 1 == i && !i.is_multiple_of(self.cols));
                if !continues {
                    if started == runs {
                        self.next = i;
                        break;
                    }
                    started += 1;
                }
                refine[i / 64] |= 1 << (i % 64);
                last = Some(i);
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

    /// A frame that carried `refinement` came out with the rate control at quality index
    /// `coded`: its blocks are clean where that put them at the target or finer. A rate control
    /// codes the rows a refinement overspends coarser, and a block it left short is refined again
    /// on its next turn, from the index it now codes at.
    pub fn refinement_coded(&mut self, refinement: &Refinement, coded: i32) {
        let at_reach = refinement.offset() <= -(refinement.reach.min(i32::MAX as u32) as i32);
        if coded + refinement.offset() <= refinement.target || at_reach {
            self.refined(&refinement.plan);
        }
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
        let plan = ledger.plan(3, usize::MAX, usize::MAX).expect("a plan");
        assert_eq!(plan.count, 4, "the two still columns of two rows");
        assert!(plan.refines(0, 0) && plan.refines(1, 1) && !plan.refines(2, 0));
        ledger.refined(&plan);
        ledger.frame([&video]);
        assert_eq!(
            ledger.plan(3, usize::MAX, usize::MAX),
            None,
            "nothing left to refine"
        );
        assert!(ledger.settled());
    }

    #[test]
    fn a_large_set_is_refined_in_turn() {
        let mut ledger = BlockLedger::new(64, 64);
        ledger.frame([]);
        let first = ledger.plan(1, 6, usize::MAX).unwrap();
        ledger.refined(&first);
        let second = ledger.plan(1, 6, usize::MAX).unwrap();
        assert_eq!((first.count, second.count), (6, 6));
        assert!(
            first.refines(0, 0) && second.refines(2, 1),
            "the second takes up where the first stopped"
        );
        ledger.refined(&second);
        let rest = ledger.plan(1, 16, usize::MAX).unwrap();
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
        assert_eq!(ledger.plan(1, 8, usize::MAX).unwrap().count, 1);
        ledger.cleaned();
        ledger.coarsened();
        assert_eq!(ledger.plan(1, 8, usize::MAX).unwrap().count, 4);
    }

    #[test]
    fn a_coarser_map_unit_refines_where_it_holds_a_refined_block() {
        let mut ledger = BlockLedger::new(64, 64);
        ledger.frame([&rect(0, 0, 64, 48)]);
        let plan = ledger.plan(1, usize::MAX, usize::MAX).unwrap();
        assert!(!plan.refines_area(0, 0, 32, 32));
        assert!(
            plan.refines_area(32, 32, 32, 32),
            "the bottom row of blocks"
        );
    }

    #[test]
    fn a_rate_that_cannot_afford_a_block_a_frame_pauses_refinement() {
        let mut pace = RefinePace::default();
        assert_eq!(pace.most(512, true), 1);
        pace.coded(true, 30, 3000, 1000.0, 512);
        assert_eq!(
            pace.most(512, true),
            0,
            "a block a frame over budget pauses"
        );
        for _ in 1..REFINE_PAUSE {
            assert_eq!(pace.most(512, true), 0);
        }
        assert_eq!(pace.most(512, true), 1, "and resumes after the pause");
        assert_eq!(
            pace.most(512, false),
            8,
            "a constant quality refines its share"
        );
    }

    #[test]
    fn a_plan_keeps_to_the_runs_an_encoder_takes() {
        let mut ledger = BlockLedger::new(64, 64);
        ledger.frame([&rect(16, 0, 16, 64)]);
        ledger.frame([&rect(16, 0, 16, 64)]);
        let plan = ledger.plan(1, usize::MAX, 3).unwrap();
        assert_eq!(
            plan.count, 4,
            "column 0 and columns 2 and 3 of row 0, then column 0 of row 1"
        );
        assert!(
            plan.refines(0, 0) && plan.refines(2, 0) && plan.refines(3, 0) && plan.refines(0, 1)
        );
        assert!(!plan.refines(2, 1));
    }

    #[test]
    fn a_block_refined_at_the_encoders_reach_counts_clean() {
        let mut ledger = BlockLedger::new(16, 16);
        ledger.frame([]);
        let plan = ledger.plan(1, 8, usize::MAX).unwrap();
        let reached = Refinement {
            plan: plan.clone(),
            target: 10,
            from: 40,
            reach: 10,
        };
        assert_eq!(reached.offset(), -10);
        ledger.refinement_coded(&reached, 40);
        assert!(ledger.settled(), "it could go no finer in one frame");
        ledger.coarsened();
        let short = Refinement {
            plan,
            target: 10,
            from: 40,
            reach: u32::MAX,
        };
        ledger.refinement_coded(&short, 45);
        assert!(!ledger.settled(), "the frame came out coarser than planned");
    }

    #[test]
    fn a_resize_starts_the_ledger_over() {
        let mut ledger = BlockLedger::new(32, 32);
        ledger.frame([]);
        ledger.cleaned();
        ledger.fit(48, 32);
        assert_eq!(ledger.blocks(), 6);
        assert!(
            ledger.plan(0, 8, usize::MAX).is_some(),
            "every block unclean"
        );
    }
}
