/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! The arithmetic a resize or a layout change passes through before any display is
//! touched: fitting a size into the server's bounds, snapping it to the encoder's
//! macroblocks, placing a second display beside the first, fitting a layout to the root
//! the server realized, deciding which windows move with their displays, and which
//! arrangement is published ahead of a layout that both adds a display and moves the
//! primary. Pure, shared by the X11 and Wayland paths and by both transports.

use std::collections::BTreeMap;

use super::{Layout, Layouts, Rect};
use crate::core::PRIMARY_DISPLAY;

/// Fit `w` by `h` inside the given bounds, preserving aspect, rounded down to even.
pub fn fit_res(w: i32, h: i32, max_w: i32, max_h: i32) -> (i32, i32) {
    if w <= max_w && h <= max_h {
        return (w, h);
    }
    let aspect = w as f64 / h as f64;
    let (mut w, mut h) = (w, h);
    if w > max_w {
        w = max_w;
        h = (w as f64 / aspect) as i32;
    }
    if h > max_h {
        h = max_h;
        w = (h as f64 * aspect) as i32;
    }
    (w - (w % 2), h - (h % 2))
}

/// Round dimensions down to multiples of 16 for `force_aligned_resolution`: encoder
/// macroblock alignment, refusing to shrink either side below 16, in which case the
/// originals come back unchanged.
pub fn align_dims_16(w: i32, h: i32) -> (i32, i32) {
    let (aw, ah) = (w - (w % 16), h - (h % 16));
    if aw >= 16 && ah >= 16 {
        (aw, ah)
    } else {
        (w, h)
    }
}

/// Parse a client resize request `WxH`, capped to the 8K ceiling a client may drive the
/// server to and rounded down to even (YUV 4:2:0 chroma alignment); `None` when
/// malformed or non-positive.
pub fn parse_resize_dims(res_str: &str) -> Option<(i32, i32)> {
    let (w, h) = res_str.split_once('x')?;
    let w: i64 = w.trim().parse().ok()?;
    let h: i64 = h.trim().parse().ok()?;
    let (w, h) = (w.min(7680) & !1, h.min(4320) & !1);
    if w <= 0 || h <= 0 {
        return None;
    }
    Some((w as i32, h as i32))
}

/// Cursor pixel size scaled from its 96-DPI base; both transports derive the X cursor
/// size from the desktop DPI with this.
pub fn cursor_size_for_dpi(dpi: f64, base_size: i32) -> i32 {
    ((dpi / 96.0 * base_size as f64).round() as i32).max(1)
}

/// What a secondary display's DPI sync is refused with on X11.
pub const DPI_FOLLOWS_PRIMARY: &str = "the desktop DPI follows the primary display";

/// Whether a DPI sync from `display_id` reaches the desktop.
///
/// On X11 one desktop carries one DPI and the primary display's page owns it; a
/// secondary's has to stop, or two windows on screens of different densities rescale the
/// whole session at every restore. On Wayland each display's screen carries its own
/// scale, so the verb passes with the display that sent it.
pub fn accepts_dpi_sync(is_wayland: bool, display_id: &str) -> bool {
    is_wayland || display_id == PRIMARY_DISPLAY
}

/// Extended-desktop layout for a primary display plus one secondary.
///
/// The secondary is placed at `position` (`right`, `left`, `up`, or `down`, anything
/// else being `right`), the same placement model on both transports, so a display looks
/// identical over either.
///
/// Returns the layouts under `primary` and `secondary` (the secondary's real id is filled
/// in by the caller) with the total size, the width rounded up to a multiple of 8
/// (xrandr framebuffer alignment).
pub fn compute_dual_layout(
    primary_wh: (i32, i32),
    secondary_wh: (i32, i32),
    position: &str,
) -> (Layouts, i32, i32) {
    let (p_w, p_h) = primary_wh;
    let (s_w, s_h) = secondary_wh;
    let mut layouts = Layouts::new();
    let (total_w, total_h) = match position {
        "left" => {
            layouts.insert("secondary".into(), Layout::new(0, 0, s_w, s_h));
            layouts.insert(PRIMARY_DISPLAY.into(), Layout::new(s_w, 0, p_w, p_h));
            (p_w + s_w, p_h.max(s_h))
        }
        "down" => {
            layouts.insert(PRIMARY_DISPLAY.into(), Layout::new(0, 0, p_w, p_h));
            layouts.insert("secondary".into(), Layout::new(0, p_h, s_w, s_h));
            (p_w.max(s_w), p_h + s_h)
        }
        "up" => {
            layouts.insert("secondary".into(), Layout::new(0, 0, s_w, s_h));
            layouts.insert(PRIMARY_DISPLAY.into(), Layout::new(0, s_h, p_w, p_h));
            (p_w.max(s_w), p_h + s_h)
        }
        _ => {
            layouts.insert(PRIMARY_DISPLAY.into(), Layout::new(0, 0, p_w, p_h));
            layouts.insert("secondary".into(), Layout::new(p_w, 0, s_w, s_h));
            (p_w + s_w, p_h.max(s_h))
        }
    };
    (layouts, (total_w + 7) & !7, total_h)
}

/// Size of the region the laid-out displays cover together: the furthest right and
/// bottom edge over all entries, either 0 when nothing bounds that axis.
pub fn layout_extent(layouts: &Layouts) -> (i32, i32) {
    layouts
        .values()
        .fold((0, 0), |(w, h), l| (w.max(l.x + l.w), h.max(l.y + l.h)))
}

/// Guard the extended-desktop layout against auto-resize feedback.
///
/// After the extend, a maximized primary client reports the full extended screen;
/// re-cropping the primary to that would span both monitors and grow the framebuffer
/// without bound. When the primary's reported size fills the current extended screen
/// along the secondary's axis, the established primary-monitor size is kept instead.
pub fn clamp_primary_feedback(
    primary_wh: (i32, i32),
    layouts: Option<&Layouts>,
    position: &str,
) -> (i32, i32) {
    let Some(prev) = layouts.and_then(|l| l.get(PRIMARY_DISPLAY)) else {
        return primary_wh;
    };
    let (p_w, p_h) = primary_wh;
    let (cur_w, cur_h) = layout_extent(layouts.unwrap_or(&Layouts::new()));
    let sideways = matches!(position, "right" | "left");
    let stacked = matches!(position, "up" | "down");
    if (sideways && p_w >= cur_w) || (stacked && p_h >= cur_h) {
        return (prev.w, prev.h);
    }
    primary_wh
}

/// What fitting a layout to the realized root changed: `dropped` displays are gone from
/// the layout, `clamped` ones kept a smaller rectangle, and `reanchored` means the primary
/// moved back to the origin (which voids the arrangement, so every secondary is dropped).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RealizedFit {
    pub dropped: Vec<String>,
    pub reanchored: bool,
    pub clamped: Vec<String>,
}

/// Fit `layouts` to the root the server actually realized, in place.
///
/// The X server, not the request, is the authority on the realized geometry: a driver can
/// reject the mode or framebuffer size and leave the root at its old dimensions, and a
/// capture region outside the root fails or grabs garbage while a pointer warp cannot
/// reach it. Rectangles are clamped inside the root; a primary that no longer fits at its
/// offset is re-anchored at the origin, which voids the arrangement and drops every
/// secondary with it. Callers own the side effects the result implies: stopping captures,
/// telling the client, and re-swapping the logical monitors.
pub fn reconcile_realized_layout(
    layouts: &mut Layouts,
    realized_w: i32,
    realized_h: i32,
) -> RealizedFit {
    let reanchored = layouts.get(PRIMARY_DISPLAY).is_some_and(|p| {
        (p.x > 0 && p.x + p.w > realized_w) || (p.y > 0 && p.y + p.h > realized_h)
    });
    if reanchored && let Some(p) = layouts.get_mut(PRIMARY_DISPLAY) {
        p.x = 0;
        p.y = 0;
    }
    let mut fit = RealizedFit {
        reanchored,
        ..RealizedFit::default()
    };
    for did in layouts.keys().cloned().collect::<Vec<_>>() {
        let layout = layouts[&did];
        if did != PRIMARY_DISPLAY
            && (reanchored || layout.x >= realized_w || layout.y >= realized_h)
        {
            layouts.remove(&did);
            fit.dropped.push(did);
            continue;
        }
        let fit_w = (layout.w.min(realized_w - layout.x) & !1).max(2);
        let fit_h = (layout.h.min(realized_h - layout.y) & !1).max(2);
        if (fit_w, fit_h) != (layout.w, layout.h) {
            let entry = layouts.get_mut(&did).expect("display present");
            entry.w = fit_w;
            entry.h = fit_h;
            fit.clamped.push(did);
        }
    }
    fit
}

/// The display whose rectangle holds `(cx, cy)`.
fn display_holding(rects: &BTreeMap<String, Rect>, cx: i32, cy: i32) -> Option<&str> {
    rects
        .iter()
        .find(|(_, r)| {
            let (bx, by, bw, bh) = **r;
            bx <= cx && cx < bx + bw && by <= cy && cy < by + bh
        })
        .map(|(did, _)| did.as_str())
}

/// Where each window goes when the displays move from `before` to `after`.
///
/// A window belongs to the display whose old rectangle holds its center. One whose display
/// moved goes with it, by the display's own move, so it keeps its place on that screen;
/// one whose display is gone goes onto the primary, at the same place within it. Either
/// is brought inside its new screen where it would poke out of it, so no title bar ends
/// up beyond an edge. A window on no display, or on one that stayed, keeps its place.
///
/// `windows` are `(window, x, y, w, h)` in root coordinates; the result is `(window, x,
/// y)` for every window that moves.
pub fn window_moves<W: Clone>(
    windows: &[(W, i32, i32, i32, i32)],
    before: &BTreeMap<String, Rect>,
    after: &BTreeMap<String, Rect>,
) -> Vec<(W, i32, i32)> {
    let primary = after.get(PRIMARY_DISPLAY);
    let mut moves = Vec::new();
    for (win, x, y, w, h) in windows {
        let (cx, cy) = (x + w / 2, y + h / 2);
        let Some(home) = display_holding(before, cx, cy) else {
            continue;
        };
        let old = before[home];
        let Some(new) = after.get(home).or(primary) else {
            continue;
        };
        let nx = (x + new.0 - old.0).clamp(new.0, new.0 + (new.2 - w).max(0));
        let ny = (y + new.1 - old.1).clamp(new.1, new.1 + (new.3 - h).max(0));
        if (nx, ny) != (*x, *y) {
            moves.push((win.clone(), nx, ny));
        }
    }
    moves
}

/// The arrangement to publish ahead of `layouts`, where it both moves the primary and
/// adds a display.
///
/// A desktop takes in a new screen at once and a moved one later: Qt announces an added
/// screen synchronously and a changed geometry through its event queue, so whatever reacts
/// to the new screen still holds the primary's old rectangle. plasmashell drops a primary
/// that a new display covers as redundant, takes it back a moment later, and its desktop
/// view never follows its screen again. Moving the primary first, on its own, gives every
/// desktop a pass in which nothing is added, and a display arriving beside a primary that
/// is already in place is an ordinary hotplug. Removing a display needs no such step.
///
/// `primary_at` is where the primary's CRTC is now (`None` when it is off) and `live` the
/// secondary displays that are outputs now. Returns the displays already live at their
/// new rectangles, or `None` when the layout adds nothing or leaves the primary where it
/// is.
pub fn output_layout_stage(
    primary_at: Option<(i32, i32)>,
    live: &[String],
    layouts: &Layouts,
) -> Option<Layouts> {
    let new = layouts.get(PRIMARY_DISPLAY)?;
    let primary_at = primary_at?;
    let is_live = |did: &str| did == PRIMARY_DISPLAY || live.iter().any(|l| l == did);
    if layouts.keys().all(|did| is_live(did)) || primary_at == (new.x, new.y) {
        return None;
    }
    Some(
        layouts
            .iter()
            .filter(|(did, _)| is_live(did))
            .map(|(d, l)| (d.clone(), *l))
            .collect(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rect(x: i32, y: i32, w: i32, h: i32) -> Layout {
        Layout::new(x, y, w, h)
    }

    fn layouts(entries: &[(&str, Layout)]) -> Layouts {
        entries.iter().map(|(d, l)| (d.to_string(), *l)).collect()
    }

    #[test]
    fn fits_inside_bounds_untouched() {
        assert_eq!(fit_res(1280, 720, 1920, 1080), (1280, 720));
        assert_eq!(fit_res(1279, 721, 1920, 1080), (1279, 721));
        assert_eq!(fit_res(1920, 1080, 1920, 1080), (1920, 1080));
    }

    #[test]
    fn fits_scale_keeping_aspect() {
        assert_eq!(fit_res(3840, 2160, 1920, 1080), (1920, 1080));
        assert_eq!(fit_res(1920, 1080, 1280, 1280), (1280, 720));
        assert_eq!(fit_res(1000, 2000, 1920, 1080), (540, 1080));
        assert_eq!(fit_res(4000, 1000, 1920, 900), (1920, 480));
        let (w, h) = fit_res(4000, 3000, 1920, 1200);
        assert_eq!((w, h), (1600, 1200));
        assert!(((w as f64 / h as f64) - 4000.0 / 3000.0).abs() < 0.001);
        assert_eq!(fit_res(1000, 1001, 999, 1000), (998, 998));
    }

    #[test]
    fn aligns_to_macroblocks() {
        assert_eq!(align_dims_16(1920, 1080), (1920, 1072));
        assert_eq!(align_dims_16(1920, 1088), (1920, 1088));
        assert_eq!(align_dims_16(16, 16), (16, 16));
        assert_eq!(align_dims_16(15, 300), (15, 300));
        assert_eq!(align_dims_16(1920, 15), (1920, 15));
    }

    #[test]
    fn parses_resize_requests() {
        assert_eq!(parse_resize_dims("1920x1080"), Some((1920, 1080)));
        assert_eq!(parse_resize_dims("1921x1081"), Some((1920, 1080)));
        assert_eq!(parse_resize_dims("9000x9000"), Some((7680, 4320)));
        assert_eq!(parse_resize_dims("0x100"), None);
        assert_eq!(parse_resize_dims("axb"), None);
        assert_eq!(parse_resize_dims("1920"), None);
    }

    #[test]
    fn dual_layout_positions() {
        let primary = (1000, 700);
        let secondary = (900, 600);
        let cases: [(&str, Layouts); 4] = [
            (
                "right",
                layouts(&[
                    ("primary", rect(0, 0, 1000, 700)),
                    ("secondary", rect(1000, 0, 900, 600)),
                ]),
            ),
            (
                "left",
                layouts(&[
                    ("secondary", rect(0, 0, 900, 600)),
                    ("primary", rect(900, 0, 1000, 700)),
                ]),
            ),
            (
                "up",
                layouts(&[
                    ("secondary", rect(0, 0, 900, 600)),
                    ("primary", rect(0, 600, 1000, 700)),
                ]),
            ),
            (
                "down",
                layouts(&[
                    ("primary", rect(0, 0, 1000, 700)),
                    ("secondary", rect(0, 700, 900, 600)),
                ]),
            ),
        ];
        for (position, want) in &cases {
            let (got, total_w, total_h) = compute_dual_layout(primary, secondary, position);
            assert_eq!(&got, want, "{position}");
            let side_by_side = matches!(*position, "left" | "right");
            assert!(
                total_w >= if side_by_side { 1900 } else { 1000 },
                "{position}"
            );
            assert!(
                total_h >= if side_by_side { 700 } else { 1300 },
                "{position}"
            );
        }
        let (got, _, _) = compute_dual_layout(primary, secondary, "sideways");
        assert_eq!(got["secondary"].x, 1000);
        assert_eq!(got["primary"].x, 0);
        let (_, total_w, total_h) = compute_dual_layout(primary, secondary, "right");
        assert_eq!((total_w, total_h), (1904, 700));
        let (_, stacked_w, stacked_h) = compute_dual_layout(primary, secondary, "down");
        assert_eq!((stacked_w, stacked_h), (1000, 1300));
    }

    #[test]
    fn extent_and_feedback_clamp() {
        let l = layouts(&[
            ("primary", rect(0, 0, 1280, 720)),
            ("d2", rect(1280, 0, 800, 600)),
        ]);
        assert_eq!(layout_extent(&l), (2080, 720));
        assert_eq!(layout_extent(&Layouts::new()), (0, 0));
        assert_eq!(
            clamp_primary_feedback((2080, 720), Some(&l), "right"),
            (1280, 720)
        );
        assert_eq!(
            clamp_primary_feedback((1000, 720), Some(&l), "right"),
            (1000, 720)
        );
        assert_eq!(
            clamp_primary_feedback((2080, 720), None, "right"),
            (2080, 720)
        );
        assert_eq!(
            clamp_primary_feedback((1280, 720), Some(&l), "down"),
            (1280, 720)
        );
    }

    #[test]
    fn stage_added_beside_moving_primary() {
        let left = layouts(&[
            ("display2", rect(0, 0, 2560, 1334)),
            ("primary", rect(2560, 0, 2560, 1280)),
        ]);
        assert_eq!(
            output_layout_stage(Some((0, 0)), &[], &left),
            Some(layouts(&[("primary", rect(2560, 0, 2560, 1280))]))
        );
        let right = layouts(&[
            ("primary", rect(0, 0, 2560, 1280)),
            ("display2", rect(2560, 0, 1920, 1046)),
        ]);
        assert_eq!(output_layout_stage(Some((0, 0)), &[], &right), None);
        assert_eq!(
            output_layout_stage(Some((0, 0)), &["display2".into()], &left),
            None
        );
        let removed = layouts(&[("primary", rect(0, 0, 2560, 1280))]);
        assert_eq!(
            output_layout_stage(Some((2560, 0)), &["display2".into()], &removed),
            None
        );
        assert_eq!(output_layout_stage(None, &[], &left), None);
        let three = layouts(&[
            ("primary", rect(1920, 0, 2560, 1280)),
            ("display3", rect(4480, 0, 1920, 1046)),
            ("display2", rect(0, 0, 1920, 1046)),
        ]);
        assert_eq!(
            output_layout_stage(Some((0, 0)), &["display3".into()], &three),
            Some(layouts(&[
                ("primary", three["primary"]),
                ("display3", three["display3"])
            ]))
        );
    }

    const PRIMARY: Rect = (0, 0, 1280, 720);

    fn rects(entries: &[(&str, Rect)]) -> BTreeMap<String, Rect> {
        entries.iter().map(|(d, r)| (d.to_string(), *r)).collect()
    }

    #[test]
    fn windows_follow_their_displays() {
        let left = rects(&[
            ("display2", (0, 0, 1024, 768)),
            ("primary", (1024, 0, 1280, 720)),
        ]);
        let up = rects(&[
            ("display2", (0, 0, 1280, 720)),
            ("primary", (0, 720, 1280, 720)),
        ]);
        let right = rects(&[("primary", PRIMARY), ("display2", (1280, 0, 1024, 768))]);
        let only_primary = rects(&[("primary", PRIMARY)]);
        let plain = ("plain", 200, 150, 400, 300);
        assert_eq!(
            window_moves(&[plain], &only_primary, &left),
            vec![("plain", 1224, 150)]
        );
        assert_eq!(
            window_moves(&[plain], &only_primary, &up),
            vec![("plain", 200, 870)]
        );
        assert_eq!(window_moves(&[plain], &only_primary, &right), vec![]);
        let moved = ("plain", 1224, 150, 400, 300);
        assert_eq!(
            window_moves(&[moved], &left, &only_primary),
            vec![("plain", 200, 150)]
        );
        let mut shrunk = left.clone();
        shrunk.insert("display2".into(), (0, 0, 800, 600));
        assert_eq!(window_moves(&[moved], &left, &shrunk), vec![]);
        let home = ("home", 100, 100, 300, 200);
        assert_eq!(window_moves(&[home], &right, &only_primary), vec![]);
        let guest = ("guest", 1380, 100, 300, 200);
        assert_eq!(
            window_moves(&[guest], &right, &only_primary),
            vec![("guest", 100, 100)]
        );
        let far = ("far", 1280 + 800, 600, 300, 200);
        assert_eq!(
            window_moves(&[far], &right, &only_primary),
            vec![("far", 800, 520)]
        );
        let stray = ("stray", 5000, 5000, 100, 100);
        assert_eq!(window_moves(&[stray], &left, &only_primary), vec![]);
        let straddling = ("straddling", 900, 100, 400, 300);
        assert_eq!(
            window_moves(&[straddling], &left, &only_primary),
            vec![("straddling", 0, 100)]
        );
    }

    #[test]
    fn realized_root_fits_layout() {
        let mut l = layouts(&[
            ("primary", rect(0, 0, 1280, 720)),
            ("d2", rect(1280, 0, 800, 600)),
        ]);
        let fit = reconcile_realized_layout(&mut l, 2080, 720);
        assert_eq!(fit, RealizedFit::default());
        assert_eq!(l["d2"], rect(1280, 0, 800, 600));

        let mut l = layouts(&[
            ("primary", rect(0, 0, 1280, 720)),
            ("d2", rect(1280, 0, 1280, 720)),
        ]);
        let fit = reconcile_realized_layout(&mut l, 1920, 720);
        assert!(fit.dropped.is_empty() && fit.clamped == ["d2"]);
        assert_eq!(l["d2"], rect(1280, 0, 640, 720));

        let mut l = layouts(&[
            ("primary", rect(0, 0, 1280, 720)),
            ("d2", rect(1280, 0, 800, 600)),
        ]);
        reconcile_realized_layout(&mut l, 1965, 720);
        assert_eq!(l["d2"].w, 684);

        let mut l = layouts(&[
            ("primary", rect(0, 0, 1280, 720)),
            ("d2", rect(1280, 0, 1280, 720)),
        ]);
        let fit = reconcile_realized_layout(&mut l, 1280, 720);
        assert!(fit.dropped == ["d2"] && !l.contains_key("d2"));

        let mut l = layouts(&[
            ("primary", rect(0, 0, 1280, 720)),
            ("d2", rect(0, 720, 1280, 720)),
        ]);
        let fit = reconcile_realized_layout(&mut l, 1280, 720);
        assert!(fit.dropped == ["d2"] && !l.contains_key("d2"));

        let mut l = layouts(&[
            ("primary", rect(1280, 0, 1280, 720)),
            ("d2", rect(0, 0, 1280, 720)),
        ]);
        let fit = reconcile_realized_layout(&mut l, 1280, 720);
        assert!(fit.reanchored && l["primary"].x == 0 && l["primary"].y == 0);
        assert!(fit.dropped == ["d2"] && !l.contains_key("d2"));

        let mut l = layouts(&[
            ("primary", rect(0, 900, 1280, 720)),
            ("d2", rect(0, 0, 1280, 720)),
        ]);
        let fit = reconcile_realized_layout(&mut l, 1280, 1000);
        assert!(fit.reanchored && fit.dropped == ["d2"]);

        let mut l = layouts(&[("primary", rect(0, 0, 1920, 1080))]);
        let fit = reconcile_realized_layout(&mut l, 1024, 768);
        assert!(fit.dropped.is_empty() && !fit.reanchored);
        assert_eq!(l["primary"], rect(0, 0, 1024, 768));

        let mut l = layouts(&[
            ("primary", rect(800, 0, 1280, 720)),
            ("d2", rect(0, 0, 800, 720)),
        ]);
        let fit = reconcile_realized_layout(&mut l, 2080, 720);
        assert!(!fit.reanchored && l["primary"].x == 800);

        let mut l = layouts(&[("primary", rect(0, 0, 1280, 720))]);
        reconcile_realized_layout(&mut l, 1, 1);
        assert!(l["primary"].w >= 2 && l["primary"].h >= 2);
    }

    #[test]
    fn cursor_size_scales_from_96() {
        assert_eq!(cursor_size_for_dpi(96.0, 32), 32);
        assert_eq!(cursor_size_for_dpi(192.0, 32), 64);
        assert_eq!(cursor_size_for_dpi(1.0, 1), 1);
    }

    #[test]
    fn dpi_sync_owner() {
        assert!(accepts_dpi_sync(false, "primary"));
        assert!(!accepts_dpi_sync(false, "display2"));
        assert!(accepts_dpi_sync(true, "display2"));
    }
}
