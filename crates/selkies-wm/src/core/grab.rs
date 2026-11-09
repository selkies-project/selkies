/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! Interactive move and resize: the geometry a drag asks for from where the pointer went, the
//! client's size hints applied to it, and openbox's edge resistance, where a frame dragged
//! against the screen's edge or another frame's stops there until the pointer pushes past the
//! resistance.

use pixelflux::wm::{Extents, Rect, ResizeEdge, SizeHints, WindowId};

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
