/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! Where the pointer is and what the pages are told about it: the laid-out displays a desktop
//! point falls on, the position the handler tracks between absolute messages, and one trackpad
//! page's pointer echo (`_PointerEcho`).
//!
//! A trackpad page draws the pointer where the server echoes it, moved on by its own deltas and
//! held inside that display after each, as the pointer is, so an echo that would leave it drawn
//! where it is goes unsent: where the page draws it, with the deltas the position includes, is
//! followed here and compared with the position. On Wayland a message counts once the
//! compositor has applied the move numbered when it was injected (`pointer_location`); X11
//! applies a move before the next request it reads.

use std::collections::{HashMap, VecDeque};

use crate::core::DisplayId;

/// A display's rectangle in the combined desktop layout.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Rect {
    pub x: i32,
    pub y: i32,
    pub w: i32,
    pub h: i32,
}

/// The displays' rectangles by id, as the transport lays them out.
pub type Layouts = HashMap<DisplayId, Rect>;

/// Size of the region the laid-out displays cover together: the furthest right and bottom edge
/// over all entries, since a layout table is normalized to a non-negative origin. Either is 0
/// when nothing bounds that axis.
pub fn layout_extent(layouts: &Layouts) -> (i32, i32) {
    layouts
        .values()
        .fold((0, 0), |(w, h), r| (w.max(r.x + r.w), h.max(r.y + r.h)))
}

/// Where a desktop point lands: the display, the point in its coordinates, and its size.
#[derive(Clone, Debug, PartialEq)]
pub struct DisplayPoint {
    pub display: DisplayId,
    pub x: f64,
    pub y: f64,
    pub w: f64,
    pub h: f64,
}

/// The display whose laid-out rectangle holds a desktop point, the point in that display's
/// coordinates, and the display's size; for a point outside them all (the framebuffer can be a
/// little wider than the layout), the nearest one, clamped into it. With no layout, the primary
/// at the point itself, unbounded.
pub fn display_at(layouts: &Layouts, x: f64, y: f64) -> DisplayPoint {
    let mut best: Option<(f64, DisplayPoint)> = None;
    let mut ids: Vec<&DisplayId> = layouts.keys().collect();
    ids.sort();
    for did in ids {
        let r = layouts[did];
        if r.w <= 0 || r.h <= 0 {
            continue;
        }
        let (rx, ry, rw, rh) = (r.x as f64, r.y as f64, r.w as f64, r.h as f64);
        if rx <= x && x < rx + rw && ry <= y && y < ry + rh {
            return DisplayPoint {
                display: did.clone(),
                x: x - rx,
                y: y - ry,
                w: rw,
                h: rh,
            };
        }
        let cx = x.max(rx).min(rx + rw - 1.0);
        let cy = y.max(ry).min(ry + rh - 1.0);
        let d = (x - cx).powi(2) + (y - cy).powi(2);
        if best.as_ref().is_none_or(|(bd, _)| d < *bd) {
            best = Some((
                d,
                DisplayPoint {
                    display: did.clone(),
                    x: cx - rx,
                    y: cy - ry,
                    w: rw,
                    h: rh,
                },
            ));
        }
    }
    best.map(|(_, p)| p).unwrap_or(DisplayPoint {
        display: crate::core::PRIMARY_DISPLAY.to_string(),
        x,
        y,
        w: f64::INFINITY,
        h: f64::INFINITY,
    })
}

/// One trackpad page's pointer echo; see the module docstring.
#[derive(Debug)]
pub struct PointerEcho {
    /// The last of the page's pointer messages the position sent includes.
    pub seq: u64,
    /// `(display, scale, w, h)` of the last echo sent.
    sent: Option<(DisplayId, f64, f64, f64)>,
    x: f64,
    y: f64,
    /// Moves injected but not yet applied by the compositor: `(motion, seq, dx, dy)`.
    pending: VecDeque<(u64, u64, f64, f64)>,
}

impl PointerEcho {
    /// An echo starting at the page's last message number.
    pub fn new(seq: u64) -> Self {
        Self {
            seq,
            sent: None,
            x: 0.0,
            y: 0.0,
            pending: VecDeque::new(),
        }
    }

    /// One of the page's pointer messages applied, with its delta (zero for an absolute
    /// position, which the page does not draw ahead of the echo) and the number of the move it
    /// ended at, None on X11.
    pub fn applied(&mut self, motion: Option<u64>, seq: u64, dx: f64, dy: f64) {
        match motion {
            None => self.include(seq, dx, dy),
            Some(m) => self.pending.push_back((m, seq, dx, dy)),
        }
    }

    fn include(&mut self, seq: u64, dx: f64, dy: f64) {
        self.seq = self.seq.max(seq);
        if let Some((_, scale, w, h)) = &self.sent {
            self.x = (self.x + dx * scale).max(0.0).min((w - 1.0).max(0.0));
            self.y = (self.y + dy * scale).max(0.0).min((h - 1.0).max(0.0));
        }
    }

    /// The last message the position of move number `motion` includes.
    pub fn through(&mut self, motion: Option<u64>) -> u64 {
        if let Some(m) = motion {
            while self.pending.front().is_some_and(|p| p.0 <= m) {
                if let Some((_, seq, dx, dy)) = self.pending.pop_front() {
                    self.include(seq, dx, dy);
                }
            }
        }
        self.seq
    }

    /// Whether the page already draws the pointer at `(x, y)` on `display`.
    pub fn drawn_at(&self, display: &str, x: f64, y: f64) -> bool {
        self.sent
            .as_ref()
            .is_some_and(|(did, _, _, _)| did == display)
            && (self.x - x).abs() < 0.5
            && (self.y - y).abs() < 0.5
    }

    /// Record the echo just sent.
    pub fn echoed(&mut self, display: &str, x: f64, y: f64, scale: f64, w: f64, h: f64) {
        self.sent = Some((display.to_string(), scale, w, h));
        self.x = x;
        self.y = y;
    }
}

/// The text of one pointer echo, as the page reads it:
/// `pointer,<display>,<x>,<y>,<scale>,<seq>` with one decimal on the position.
pub fn echo_message(display: &str, x: f64, y: f64, scale: f64, seq: u64) -> String {
    format!("pointer,{display},{x:.1},{y:.1},{},{seq}", format_g(scale))
}

/// A float as `%g` prints it for the scales a display reports (1, 1.5, 2).
pub fn format_g(value: f64) -> String {
    if value.fract() == 0.0 && value.abs() < 1e15 {
        format!("{}", value as i64)
    } else {
        let s = format!("{value:.6}");
        s.trim_end_matches('0').trim_end_matches('.').to_string()
    }
}

/// The value a page reads back from one decimal of text.
pub fn one_decimal(value: f64) -> f64 {
    format!("{value:.1}").parse().unwrap_or(value)
}

/// The value a page reads back from a `%g` of text.
pub fn g_rounded(value: f64) -> f64 {
    format_g(value).parse().unwrap_or(value)
}

/// Client button-mask bits translated before the per-bit diff: the pen eraser (Pointer Events
/// button 5) drives the primary button.
pub const MOUSE_MASK_BIT_PRIMARY: u32 = 1 << 0;
pub const MOUSE_MASK_BIT_ERASER: u32 = 1 << 5;
/// The wheel's bits: 3 and 4 vertical, 6 and 7 horizontal, each a click on its rising edge
/// while the message carries a scroll magnitude.
pub const MOUSE_MASK_SCROLL_BITS: u32 = (1 << 3) | (1 << 4) | (1 << 6) | (1 << 7);

/// The pointer state the handler keeps between messages (`send_x11_mouse`'s half that decides
/// rather than injects).
#[derive(Debug, Default)]
pub struct PointerTracker {
    /// The buttons held, in the client's bit numbering.
    pub button_mask: u32,
    pub last_x: i32,
    pub last_y: i32,
    /// Whether a delta moved the tracked position since the last absolute message, so the next
    /// absolute position warps unconditionally.
    pub stale: bool,
}

/// What one pointer message resolved to.
#[derive(Debug, PartialEq)]
pub struct PointerMove {
    /// The position to warp to, or the delta to move by.
    pub x: i32,
    pub y: i32,
    pub relative: bool,
    /// Whether the message changed the position (an absolute one) or left it stale.
    pub position_changed: bool,
    /// A zero delta, which injects no motion.
    pub static_relative: bool,
}

impl PointerTracker {
    /// A tracker whose first absolute position always warps.
    pub fn new() -> Self {
        Self {
            button_mask: 0,
            last_x: -1,
            last_y: -1,
            stale: false,
        }
    }

    /// Resolve one pointer message against the layout and move the tracked position.
    ///
    /// A delta is injected as a delta; the tracked position follows it only so that an absolute
    /// message has something to compare against, and it is bounded by the laid-out screen
    /// because the X server and the compositor bound the pointer the same way. The tracking
    /// stays an estimate even so (an application can move the pointer, and the framebuffer can
    /// be a few pixels wider than the layout), so the first absolute position after any delta
    /// warps unconditionally rather than trusting that comparison. Where no relative injection
    /// exists, the bound matters directly: the estimate is what gets injected. An absolute
    /// position for a secondary with no laid-out region is None: its clicks would land on the
    /// primary at offset zero, and a held button self-heals on the next mask diff.
    pub fn resolve(
        &mut self,
        x: i32,
        y: i32,
        relative: bool,
        display_id: &str,
        layouts: &Layouts,
    ) -> Option<PointerMove> {
        let was_stale = self.stale;
        let (final_x, final_y) = if relative {
            // XTEST carries the delta in an Int16; an overlarge value would fail the request
            // and take the button transitions on this message with it.
            let x = x.clamp(-32768, 32767);
            let y = y.clamp(-32768, 32767);
            let (mut fx, mut fy) = (self.last_x.saturating_add(x), self.last_y.saturating_add(y));
            let (edge_x, edge_y) = layout_extent(layouts);
            if edge_x > 0 {
                fx = fx.clamp(0, edge_x - 1);
            }
            if edge_y > 0 {
                fy = fy.clamp(0, edge_y - 1);
            }
            self.stale = true;
            let changed = was_stale || fx != self.last_x || fy != self.last_y;
            self.last_x = fx;
            self.last_y = fy;
            return Some(PointerMove {
                x,
                y,
                relative: true,
                position_changed: changed,
                static_relative: x == 0 && y == 0,
            });
        } else {
            let lookup = if display_id.is_empty() {
                crate::core::PRIMARY_DISPLAY
            } else {
                display_id
            };
            let (ox, oy) = match layouts.get(lookup) {
                Some(r) => (r.x, r.y),
                None if !layouts.is_empty() && lookup != crate::core::PRIMARY_DISPLAY => {
                    return None;
                }
                None => (0, 0),
            };
            self.stale = false;
            (x.saturating_add(ox), y.saturating_add(oy))
        };
        let changed = was_stale || final_x != self.last_x || final_y != self.last_y;
        self.last_x = final_x;
        self.last_y = final_y;
        Some(PointerMove {
            x: final_x,
            y: final_y,
            relative: false,
            position_changed: changed,
            static_relative: false,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn layouts(entries: &[(&str, i32, i32, i32, i32)]) -> Layouts {
        entries
            .iter()
            .map(|(id, x, y, w, h)| {
                (
                    id.to_string(),
                    Rect {
                        x: *x,
                        y: *y,
                        w: *w,
                        h: *h,
                    },
                )
            })
            .collect()
    }

    /// `test_pointer_tracking.py`: the extent of the laid-out displays.
    #[test]
    fn extents() {
        let single = layouts(&[("primary", 0, 0, 1920, 1080)]);
        let dual = layouts(&[
            ("primary", 0, 0, 1920, 1080),
            ("second", 1920, 0, 1280, 720),
        ]);
        assert_eq!(layout_extent(&single), (1920, 1080));
        assert_eq!(layout_extent(&dual), (3200, 1080));
        assert_eq!(layout_extent(&Layouts::new()), (0, 0));
        assert_eq!(
            layout_extent(&layouts(&[("primary", 0, 0, 1920, 0)])),
            (1920, 0),
            "a null field costs only its own axis"
        );
    }

    #[test]
    fn tracked_position_stays_inside_the_layout() {
        let single = layouts(&[("primary", 0, 0, 1920, 1080)]);
        let mut t = PointerTracker::new();
        t.resolve(900, 500, false, "primary", &single).unwrap();
        let m = t.resolve(4000, 0, true, "primary", &single).unwrap();
        assert_eq!(
            (m.x, m.y, m.relative),
            (4000, 0, true),
            "a backend with relative injection gets the delta verbatim"
        );
        assert_eq!(
            t.last_x, 1919,
            "the tracked position still stops at the edge"
        );
        t.resolve(-100, 0, true, "primary", &single).unwrap();
        assert_eq!(
            t.last_x, 1819,
            "a delta past the edge leaves the pointer one step back from it"
        );
        let m = t.resolve(99999, 0, true, "primary", &single).unwrap();
        assert_eq!(
            m.x, 32767,
            "a delta too large for the request is saturated, not dropped"
        );
        let mut t = PointerTracker::new();
        t.resolve(900, 500, false, "primary", &Layouts::new())
            .unwrap();
        t.resolve(4000, 0, true, "primary", &Layouts::new())
            .unwrap();
        assert_eq!(t.last_x, 4900, "no layout means no bound, and no failure");
        let dual = layouts(&[
            ("primary", 0, 0, 1920, 1080),
            ("second", 1920, 0, 1280, 720),
        ]);
        let mut t = PointerTracker::new();
        t.resolve(900, 500, false, "primary", &dual).unwrap();
        t.resolve(4000, 0, true, "primary", &dual).unwrap();
        assert_eq!(
            t.last_x, 3199,
            "the bound follows the whole extended desktop"
        );
        assert!(
            t.resolve(1, 1, false, "third", &dual).is_none(),
            "a secondary with no laid-out region injects nothing"
        );
        let m = t
            .resolve(i32::MAX, i32::MIN, false, "second", &dual)
            .unwrap();
        assert_eq!(
            (m.x, m.y),
            (i32::MAX, i32::MIN),
            "a position off every scale saturates"
        );
        let m = t.resolve(10, 10, false, "", &dual).unwrap();
        assert_eq!((m.x, m.y), (10, 10), "no display renders the primary");
    }

    #[test]
    fn first_absolute_after_deltas_warps() {
        let single = layouts(&[("primary", 0, 0, 1920, 1080)]);
        let mut t = PointerTracker::new();
        t.resolve(900, 500, false, "primary", &single).unwrap();
        t.resolve(300, 0, true, "primary", &single).unwrap();
        t.resolve(-300, 0, true, "primary", &single).unwrap();
        let m = t.resolve(900, 500, false, "primary", &single).unwrap();
        assert!(
            m.position_changed,
            "an absolute position after deltas warps even where it was tracked"
        );
        let m = t.resolve(900, 500, false, "primary", &single).unwrap();
        assert!(!m.position_changed, "a repeat of it does not warp again");
        let m = t.resolve(0, 0, true, "primary", &single).unwrap();
        assert!(m.static_relative, "a zero delta injects no motion");
    }

    #[test]
    fn display_lookup_clamps_to_the_nearest() {
        let l = layouts(&[
            ("primary", 0, 0, 1920, 1080),
            ("display2", 1920, 0, 1280, 720),
        ]);
        let p = display_at(&l, 2000.0, 50.0);
        assert_eq!((p.display.as_str(), p.x, p.y), ("display2", 80.0, 50.0));
        let p = display_at(&l, 3500.0, 900.0);
        assert_eq!((p.display.as_str(), p.x, p.y), ("display2", 1279.0, 719.0));
        let p = display_at(&Layouts::new(), 5.0, 6.0);
        assert_eq!((p.display.as_str(), p.x, p.y), ("primary", 5.0, 6.0));
    }

    #[test]
    fn echo_bookkeeping() {
        let mut e = PointerEcho::new(0);
        e.echoed("primary", 100.0, 100.0, 1.0, 1920.0, 1080.0);
        e.applied(Some(1), 1, 10.0, 5.0);
        assert_eq!(
            e.through(Some(0)),
            0,
            "a move not yet applied is not included"
        );
        assert_eq!(e.through(Some(1)), 1);
        assert!(e.drawn_at("primary", 110.0, 105.0));
        assert!(!e.drawn_at("display2", 110.0, 105.0));
        e.applied(None, 2, 5000.0, 0.0);
        assert!(
            e.drawn_at("primary", 1919.0, 105.0),
            "the page holds the pointer inside the display"
        );
        assert_eq!(
            echo_message("primary", 100.0, 100.0, 1.0, 0),
            "pointer,primary,100.0,100.0,1,0"
        );
        assert_eq!(
            echo_message("display2", 80.0, 50.0, 2.0, 3),
            "pointer,display2,80.0,50.0,2,3"
        );
        assert_eq!(format_g(1.5), "1.5");
    }
}
