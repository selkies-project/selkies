/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! Interactive move and resize: the geometry a drag asks for from where the pointer went, the
//! client's size hints applied to it, openbox's edge resistance, where a frame dragged against
//! the screen's edge or another frame's stops there until the pointer pushes past the resistance,
//! and the quick-tile zones a move drag reaches at the output's edges.
//!
//! The zones are the common desktops': the pointer at the left or right edge of the output tiles
//! the window to that half, in the top or bottom fifth of such an edge to that quarter, and at
//! the top edge maximizes it. The zone follows the pointer, not the frame, so a window dragged
//! by a far corner still tiles where the hand went. A narrow output (a phone held upright) has
//! room for one window across, so every edge there maximizes.

use pixelflux::wm::{Extents, Rect, ResizeEdge, SizeHints, WindowId};

/// A quick-tile zone: where a tiled window lies on its output's work area.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Zone {
    Left,
    Right,
    TopLeft,
    TopRight,
    BottomLeft,
    BottomRight,
    /// The whole work area: the top edge's zone, applied as a maximize.
    Maximize,
}

/// How close to an output's edge, in logical pixels, the pointer must be to reach a zone.
const ZONE_EDGE: i32 = 8;
/// Below this width an output takes one window across, so every zone is the whole area.
const NARROW_OUTPUT: i32 = 700;

/// The zone the pointer at `(x, y)` reaches on an output of `output` logical pixels, if any.
pub fn zone_at(output: Rect, x: f64, y: f64) -> Option<Zone> {
    let (x, y) = (x.round() as i32, y.round() as i32);
    if !output.contains(
        x.clamp(output.x, output.right() - 1),
        y.clamp(output.y, output.bottom() - 1),
    ) {
        return None;
    }
    let left = x <= output.x + ZONE_EDGE;
    let right = x >= output.right() - 1 - ZONE_EDGE;
    let top = y <= output.y + ZONE_EDGE;
    let fifth = output.h / 5;
    let upper = y < output.y + fifth;
    let lower = y >= output.bottom() - fifth;
    if output.w < NARROW_OUTPUT {
        return (left || right || top).then_some(Zone::Maximize);
    }
    Some(match (left, right, top) {
        (true, _, _) if upper => Zone::TopLeft,
        (true, _, _) if lower => Zone::BottomLeft,
        (true, _, _) => Zone::Left,
        (_, true, _) if upper => Zone::TopRight,
        (_, true, _) if lower => Zone::BottomRight,
        (_, true, _) => Zone::Right,
        (_, _, true) => Zone::Maximize,
        _ => return None,
    })
}

/// The frame rectangle a zone covers on a work area.
pub fn zone_rect(zone: Zone, area: Rect) -> Rect {
    let half_w = area.w / 2;
    let half_h = area.h / 2;
    let right_x = area.x + half_w;
    let right_w = area.w - half_w;
    let bottom_y = area.y + half_h;
    let bottom_h = area.h - half_h;
    match zone {
        Zone::Left => Rect::new(area.x, area.y, half_w, area.h),
        Zone::Right => Rect::new(right_x, area.y, right_w, area.h),
        Zone::TopLeft => Rect::new(area.x, area.y, half_w, half_h),
        Zone::TopRight => Rect::new(right_x, area.y, right_w, half_h),
        Zone::BottomLeft => Rect::new(area.x, bottom_y, half_w, bottom_h),
        Zone::BottomRight => Rect::new(right_x, bottom_y, right_w, bottom_h),
        Zone::Maximize => area,
    }
}

/// The zone a keyboard snap toward `edge` moves a window to from its current one: left and
/// right take that half, or move a quarter across; up takes a window from
/// a bottom quarter to its half, from a half to its top quarter, and otherwise maximizes; down
/// takes a top quarter to its half, a half to its bottom quarter, and otherwise restores (the
/// caller minimizes a window that has nothing to restore). `None` means restore.
pub fn snapped(current: Option<Zone>, edge: ResizeEdge) -> Option<Zone> {
    use Zone::*;
    match edge {
        ResizeEdge::Left => Some(match current {
            Some(TopRight) => TopLeft,
            Some(BottomRight) => BottomLeft,
            _ => Left,
        }),
        ResizeEdge::Right => Some(match current {
            Some(TopLeft) => TopRight,
            Some(BottomLeft) => BottomRight,
            _ => Right,
        }),
        ResizeEdge::Top => Some(match current {
            Some(BottomLeft) => Left,
            Some(BottomRight) => Right,
            Some(Left) => TopLeft,
            Some(Right) => TopRight,
            _ => Maximize,
        }),
        ResizeEdge::Bottom => match current {
            Some(TopLeft) => Some(Left),
            Some(TopRight) => Some(Right),
            Some(Left) => Some(BottomLeft),
            Some(Right) => Some(BottomRight),
            _ => None,
        },
        _ => current,
    }
}

/// What a drag does.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GrabKind {
    Move,
    Resize(ResizeEdge),
}

/// A drag in progress on one window.
#[derive(Clone, Debug, PartialEq)]
pub struct Grab {
    pub window: WindowId,
    pub kind: GrabKind,
    /// Where the pointer was when the drag began.
    pub origin: (f64, f64),
    /// The client area when the drag began.
    pub start: Rect,
    /// The button that holds the drag, released to end it; `None` for a drag the keyboard began.
    pub button: Option<u32>,
}

/// The client rectangle a drag asks for with the pointer at `(x, y)`, from the start geometry.
pub fn dragged(grab: &Grab, x: f64, y: f64) -> Rect {
    let dx = (x - grab.origin.0).round() as i32;
    let dy = (y - grab.origin.1).round() as i32;
    let s = grab.start;
    match grab.kind {
        GrabKind::Move => s.at(s.x + dx, s.y + dy),
        GrabKind::Resize(edge) => {
            let mut r = s;
            if edge.left() {
                r.x = s.x + dx;
                r.w = s.w - dx;
            } else if edge.right() {
                r.w = s.w + dx;
            }
            if edge.top() {
                r.y = s.y + dy;
                r.h = s.h - dy;
            } else if edge.bottom() {
                r.h = s.h + dy;
            }
            r
        }
    }
}

/// The edge of a resize the pointer's position inside the client picks when a resize names
/// none: the nearest corner, as openbox's `Resize` without an edge does.
pub fn edge_for(rect: Rect, x: f64, y: f64) -> ResizeEdge {
    let left = x - (rect.x as f64) < rect.w as f64 / 2.0;
    let top = y - (rect.y as f64) < rect.h as f64 / 2.0;
    match (top, left) {
        (true, true) => ResizeEdge::TopLeft,
        (true, false) => ResizeEdge::TopRight,
        (false, true) => ResizeEdge::BottomLeft,
        (false, false) => ResizeEdge::BottomRight,
    }
}

/// Smallest a client is ever sized.
pub const MIN_SIZE: i32 = 32;

/// A client rectangle with the size hints applied: the minimum and maximum, then the increments
/// from the base size, the side the edge keeps still staying still.
pub fn constrain(rect: Rect, hints: &SizeHints, edge: ResizeEdge) -> Rect {
    let mut r = rect;
    let (min_w, min_h) = hints
        .min
        .map(|m| (m.w.max(1), m.h.max(1)))
        .unwrap_or((MIN_SIZE, MIN_SIZE));
    let (max_w, max_h) = hints
        .max
        .map(|m| (m.w.max(min_w), m.h.max(min_h)))
        .unwrap_or((i32::MAX, i32::MAX));
    let mut w = r.w.clamp(min_w.max(1), max_w);
    let mut h = r.h.clamp(min_h.max(1), max_h);
    if let Some(inc) = hints.increment {
        let base = hints.base.or(hints.min).unwrap_or_default();
        let snap = |value: i32, base: i32, inc: i32, min: i32| {
            let down = base + ((value - base).max(0) / inc) * inc;
            if down < min {
                base + ((min - base).max(0) + inc - 1) / inc * inc
            } else {
                down
            }
        };
        if inc.w > 0 {
            w = snap(w, base.w, inc.w, min_w.max(1));
        }
        if inc.h > 0 {
            h = snap(h, base.h, inc.h, min_h.max(1));
        }
    }
    if edge.left() {
        r.x += r.w - w;
    }
    if edge.top() {
        r.y += r.h - h;
    }
    r.w = w;
    r.h = h;
    r
}

/// A moved frame held at an edge it is dragged against: the screen's edges with
/// `screen_strength`, and the frames of `others` with `strength`, each only while the overshoot
/// is within the strength. The result is the frame's new position.
pub fn resist_move(frame: Rect, area: Rect, others: &[Rect], strength: i32, screen_strength: i32) -> Rect {
    let mut r = frame;
    if screen_strength > 0 {
        if r.x < area.x && r.x > area.x - screen_strength {
            r.x = area.x;
        }
        if r.right() > area.right() && r.right() < area.right() + screen_strength {
            r.x = area.right() - r.w;
        }
        if r.y < area.y && r.y > area.y - screen_strength {
            r.y = area.y;
        }
        if r.bottom() > area.bottom() && r.bottom() < area.bottom() + screen_strength {
            r.y = area.bottom() - r.h;
        }
    }
    if strength > 0 {
        for other in others {
            let overlaps_y = r.y < other.bottom() && r.bottom() > other.y;
            let overlaps_x = r.x < other.right() && r.right() > other.x;
            if overlaps_y {
                if r.right() > other.x && r.right() <= other.x + strength && frame.x < other.x {
                    r.x = other.x - r.w;
                } else if r.x < other.right()
                    && r.x >= other.right() - strength
                    && frame.right() > other.right()
                {
                    r.x = other.right();
                }
            }
            if overlaps_x {
                if r.bottom() > other.y && r.bottom() <= other.y + strength && frame.y < other.y {
                    r.y = other.y - r.h;
                } else if r.y < other.bottom()
                    && r.y >= other.bottom() - strength
                    && frame.bottom() > other.bottom()
                {
                    r.y = other.bottom();
                }
            }
        }
    }
    r
}

/// A client rectangle from a frame rectangle, the extents taken off.
pub fn client_of(frame: Rect, extents: Extents) -> Rect {
    Rect::new(
        frame.x + extents.left,
        frame.y + extents.top,
        frame.w - extents.left - extents.right,
        frame.h - extents.top - extents.bottom,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use pixelflux::wm::Size;

    fn grab(kind: GrabKind) -> Grab {
        Grab {
            window: 1,
            kind,
            origin: (100.0, 100.0),
            start: Rect::new(50, 60, 400, 300),
            button: Some(1),
        }
    }

    /// A move follows the pointer; a resize from a corner moves that corner alone.
    #[test]
    fn drags_follow_the_pointer() {
        assert_eq!(
            dragged(&grab(GrabKind::Move), 130.0, 90.0),
            Rect::new(80, 50, 400, 300)
        );
        assert_eq!(
            dragged(&grab(GrabKind::Resize(ResizeEdge::TopLeft)), 110.0, 120.0),
            Rect::new(60, 80, 390, 280)
        );
        assert_eq!(
            dragged(&grab(GrabKind::Resize(ResizeEdge::BottomRight)), 110.0, 120.0),
            Rect::new(50, 60, 410, 320)
        );
        assert_eq!(
            edge_for(Rect::new(0, 0, 100, 100), 90.0, 10.0),
            ResizeEdge::TopRight
        );
    }

    /// Size hints bound the size and snap it to increments, keeping the still edge where it is.
    #[test]
    fn hints_bound_and_snap_the_size() {
        let hints = SizeHints {
            min: Some(Size { w: 100, h: 80 }),
            max: Some(Size { w: 800, h: 600 }),
            increment: Some(Size { w: 10, h: 20 }),
            base: Some(Size { w: 4, h: 6 }),
        };
        let r = constrain(Rect::new(0, 0, 55, 700), &hints, ResizeEdge::BottomRight);
        assert_eq!(r, Rect::new(0, 0, 104, 586));
        let left = constrain(Rect::new(100, 100, 55, 300), &hints, ResizeEdge::Left);
        assert_eq!(left.right(), 155);
        assert_eq!(left.w, 104);
        assert_eq!(
            constrain(Rect::new(0, 0, 5, 5), &SizeHints::default(), ResizeEdge::None).w,
            MIN_SIZE
        );
    }

    /// The pointer at an output's edges reaches the halves, the quarters at the ends of the
    /// side edges, and the maximize zone at the top; a narrow output maximizes everywhere.
    #[test]
    fn zones_follow_the_pointer() {
        let output = Rect::new(0, 0, 1920, 1080);
        assert_eq!(zone_at(output, 2.0, 500.0), Some(Zone::Left));
        assert_eq!(zone_at(output, 1919.0, 500.0), Some(Zone::Right));
        assert_eq!(zone_at(output, 0.0, 10.0), Some(Zone::TopLeft));
        assert_eq!(zone_at(output, 1915.0, 1079.0), Some(Zone::BottomRight));
        assert_eq!(zone_at(output, 900.0, 0.0), Some(Zone::Maximize));
        assert_eq!(zone_at(output, 900.0, 1079.0), None);
        assert_eq!(zone_at(output, 900.0, 500.0), None);
        assert_eq!(
            zone_at(Rect::new(0, 0, 480, 900), 0.0, 450.0),
            Some(Zone::Maximize)
        );
        let area = Rect::new(0, 0, 1000, 800);
        assert_eq!(zone_rect(Zone::Right, area), Rect::new(500, 0, 500, 800));
        assert_eq!(zone_rect(Zone::BottomLeft, area), Rect::new(0, 400, 500, 400));
        assert_eq!(snapped(None, ResizeEdge::Left), Some(Zone::Left));
        assert_eq!(snapped(Some(Zone::Left), ResizeEdge::Top), Some(Zone::TopLeft));
        assert_eq!(snapped(Some(Zone::TopLeft), ResizeEdge::Bottom), Some(Zone::Left));
        assert_eq!(
            snapped(Some(Zone::Left), ResizeEdge::Bottom),
            Some(Zone::BottomLeft)
        );
        assert_eq!(snapped(Some(Zone::BottomLeft), ResizeEdge::Bottom), None);
        assert_eq!(snapped(None, ResizeEdge::Top), Some(Zone::Maximize));
        assert_eq!(
            snapped(Some(Zone::TopRight), ResizeEdge::Left),
            Some(Zone::TopLeft)
        );
    }

    /// A frame dragged a little past the screen's edge or against another frame holds there,
    /// and one pushed further goes through.
    #[test]
    fn resistance_holds_at_edges() {
        let area = Rect::new(0, 0, 1000, 800);
        let other = Rect::new(500, 0, 300, 300);
        assert_eq!(resist_move(Rect::new(-5, 10, 100, 100), area, &[], 10, 20).x, 0);
        assert_eq!(
            resist_move(Rect::new(-25, 10, 100, 100), area, &[], 10, 20).x,
            -25
        );
        assert_eq!(
            resist_move(Rect::new(405, 10, 100, 100), area, &[other], 10, 20).x,
            400
        );
        assert_eq!(
            resist_move(Rect::new(405, 400, 100, 100), area, &[other], 10, 20).x,
            405
        );
    }
}
