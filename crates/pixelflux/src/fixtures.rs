/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! Screen content the measurements draw, the same on every run: a document that scrolls
//! (`Canvas`), a moving picture (`paint_video`), and the blocks a frame changed
//! (`changed_blocks`), as a source that reports where it drew hands them over. The cleanup
//! measurements (`cleanup_bench`) and the encode benchmarks (`benches/encode.rs`) drive the
//! pipeline with them; nothing in a capture reads them.

use smithay::utils::{Physical, Rectangle};

/// A document taller than the screen: dense text in a few colors on white, a title bar, and a
/// gradient side panel, so a scroll moves detail an encoder has to spend on.
pub struct Canvas {
    pub(crate) w: usize,
    pub(crate) h: usize,
    pub(crate) bgra: Vec<u8>,
}

impl Canvas {
    /// A `w` x `h` document, which a screen scrolled past its last row wraps back to the top of.
    pub fn new(w: usize, h: usize) -> Self {
        let mut bgra = vec![255u8; w * h * 4];
        let colors: [[u8; 3]; 4] = [[20, 20, 20], [160, 40, 30], [30, 110, 40], [40, 40, 170]];
        for y in 0..h {
            let (cell_y, gy) = (y / 16, y % 16);
            for x in 0..w {
                let i = (y * w + x) * 4;
                if x < w / 8 {
                    let g = (y * 255 / h) as u8;
                    bgra[i] = 200 - g / 3;
                    bgra[i + 1] = 180;
                    bgra[i + 2] = 120 + g / 3;
                    continue;
                }
                let (cell_x, gx) = (x / 9, x % 9);
                if gy >= 12 || gx >= 7 {
                    continue;
                }
                let mut s = (cell_x as u32)
                    .wrapping_mul(2654435761)
                    .wrapping_add((cell_y as u32).wrapping_mul(40503))
                    .wrapping_add(1);
                s ^= s << 13;
                s ^= s >> 17;
                s ^= s << 5;
                if (cell_x + cell_y * 7) % 23 == 0 {
                    continue;
                }
                if (s >> ((gy * 3 + gx) % 29)) & 1 == 1 {
                    let c = colors[((cell_y / 3) + cell_x / 40) % 4];
                    bgra[i] = c[2];
                    bgra[i + 1] = c[1];
                    bgra[i + 2] = c[0];
                }
            }
        }
        Self { w, h, bgra }
    }

    /// Draw into `f` the screen at scroll offset `scroll` rows, with a title bar and, when
    /// `caret`, a caret. The callers reuse one buffer, as a capture does: an NVENC session
    /// page-locks a host source once, by its address.
    pub fn frame(&self, f: &mut [u8], sh: usize, scroll: usize, caret: bool) {
        let row = self.w * 4;
        for y in 0..sh {
            let src = (y + scroll) % self.h;
            f[y * row..(y + 1) * row].copy_from_slice(&self.bgra[src * row..(src + 1) * row]);
        }
        for y in 0..28.min(sh) {
            for x in 0..self.w {
                let i = y * row + x * 4;
                f[i] = 90;
                f[i + 1] = 60;
                f[i + 2] = 40;
            }
        }
        if caret {
            let (cx, cy) = (self.w / 2 + 3, sh / 2);
            for y in cy..(cy + 18).min(sh) {
                for x in cx..cx + 2 {
                    let i = y * row + x * 4;
                    f[i] = 0;
                    f[i + 1] = 0;
                    f[i + 2] = 0;
                }
            }
        }
    }
}

/// A moving picture in `area` (`x`, `y`, `width`, `height`) of the `w`-wide BGRA `frame` at
/// frame `t`: bands that drift a few pixels a frame under a grain that changes every frame, so
/// a predicted frame has motion to code and texture it cannot predict.
pub fn paint_video(
    frame: &mut [u8],
    w: usize,
    (x0, y0, vw, vh): (usize, usize, usize, usize),
    t: usize,
) {
    let mut seed = (t as u32).wrapping_mul(0x9e37_79b9) | 1;
    for y in y0..y0 + vh {
        for x in x0..x0 + vw {
            seed ^= seed << 13;
            seed ^= seed >> 17;
            seed ^= seed << 5;
            let grain = (seed & 15) as usize;
            let band = ((x - x0) + (y - y0) / 2 + t * 3) % 256;
            let o = (y * w + x) * 4;
            frame[o] = (band + grain) as u8;
            frame[o + 1] = ((band * 3 / 4) + (y - y0) / 4 + grain) as u8;
            frame[o + 2] = (255 - band / 2 + grain) as u8;
            frame[o + 3] = 255;
        }
    }
}

/// The 16x16 blocks of the `w` x `h` BGRA `frame` that differ from `previous`, as rectangles a
/// run of them along a row: what a source that reports exactly where it drew says.
pub fn changed_blocks(
    frame: &[u8],
    previous: &[u8],
    w: usize,
    h: usize,
) -> Vec<Rectangle<i32, Physical>> {
    let mut rects = Vec::new();
    for by in (0..h).step_by(16) {
        let rows = by..(by + 16).min(h);
        let mut run: Option<usize> = None;
        for bx in (0..w).step_by(16).chain(std::iter::once(w)) {
            let changed = bx < w
                && rows.clone().any(|y| {
                    let (a, b) = (y * w * 4 + bx * 4, y * w * 4 + (bx + 16).min(w) * 4);
                    frame[a..b] != previous[a..b]
                });
            match (changed, run) {
                (true, None) => run = Some(bx),
                (false, Some(x0)) => {
                    rects.push(Rectangle::new(
                        (x0 as i32, by as i32).into(),
                        ((bx - x0) as i32, rows.len() as i32).into(),
                    ));
                    run = None;
                }
                _ => {}
            }
        }
    }
    rects
}
