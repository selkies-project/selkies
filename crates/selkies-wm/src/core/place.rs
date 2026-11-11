/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! Where a new window goes: openbox's `Smart` placement, which puts a frame where it overlaps
//! the least of what is already shown, `Cascade`, `Center`, and `UnderMouse`, each inside the
//! output's workarea, and the clamp that keeps any frame reachable.

use pixelflux::wm::Rect;

use crate::config::PlacementPolicy;

/// The frame rectangle `frame` moved so it lies inside `area` as far as it fits, its top-left
/// corner kept reachable when it does not.
pub fn clamp(frame: Rect, area: Rect) -> Rect {
    let mut x = frame.x;
    let mut y = frame.y;
    if x + frame.w > area.right() {
        x = area.right() - frame.w;
    }
    if y + frame.h > area.bottom() {
        y = area.bottom() - frame.h;
    }
    x = x.max(area.x);
    y = y.max(area.y);
    frame.at(x, y)
}

/// The spot for a frame of `size` on `area` by the policy, the frames already there given so the
/// smart policy avoids them and the cascade steps past the last; `pointer` is where the pointer
/// is for the under-mouse policy.
pub fn place(
    policy: PlacementPolicy,
    size: (i32, i32),
    area: Rect,
    others: &[Rect],
    pointer: (i32, i32),
    cascade_step: i32,
) -> Rect {
    let (w, h) = size;
    let frame = Rect::new(0, 0, w, h);
    match policy {
        PlacementPolicy::Center => clamp(
            frame.at(area.x + (area.w - w) / 2, area.y + (area.h - h) / 2),
            area,
        ),
        PlacementPolicy::UnderMouse => clamp(frame.at(pointer.0 - w / 2, pointer.1 - h / 2), area),
        PlacementPolicy::Cascade => {
            let last = others.last();
            let (x, y) = match last {
                Some(last) => (last.x + cascade_step, last.y + cascade_step),
                None => (area.x + cascade_step, area.y + cascade_step),
            };
            let candidate = frame.at(x, y);
            if candidate.right() > area.right() || candidate.bottom() > area.bottom() {
                clamp(frame.at(area.x, area.y), area)
            } else {
                candidate
            }
        }
        PlacementPolicy::Smart => smart(frame, area, others),
    }
}

/// The spot overlapping the least of `others`: every candidate corner (the area's, and each
/// other frame's outer edges) is tried, the smallest overlap wins, and among equals the
/// highest, then the leftmost, so a row fills before the next starts.
fn smart(frame: Rect, area: Rect, others: &[Rect]) -> Rect {
    let mut xs = vec![area.x, area.right() - frame.w];
    let mut ys = vec![area.y, area.bottom() - frame.h];
    for other in others {
        xs.push(other.right());
        xs.push(other.x - frame.w);
        ys.push(other.bottom());
        ys.push(other.y - frame.h);
    }
    xs.retain(|&x| x >= area.x && x + frame.w <= area.right());
    ys.retain(|&y| y >= area.y && y + frame.h <= area.bottom());
    xs.sort_unstable();
    xs.dedup();
    ys.sort_unstable();
    ys.dedup();
    if xs.is_empty() || ys.is_empty() {
        return clamp(frame.at(area.x, area.y), area);
    }
    let mut best: Option<(i64, i64, Rect)> = None;
    for &y in &ys {
        for &x in &xs {
            let candidate = frame.at(x, y);
            let overlap: i64 = others.iter().map(|o| candidate.intersection(o).area()).sum();
            let distance = (y - area.y) as i64 * (area.w as i64 + 1) + (x - area.x) as i64;
            let better = match &best {
                None => true,
                Some((o, d, _)) => overlap < *o || (overlap == *o && distance < *d),
            };
            if better {
                best = Some((overlap, distance, candidate));
            }
            if overlap == 0 && distance == 0 {
                return candidate;
            }
        }
    }
    best.map(|(_, _, r)| r).unwrap_or(frame)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The smart policy finds the free spot beside a window and falls to the least overlap when
    /// the area is full; the clamp keeps a frame inside the area.
    #[test]
    fn smart_placement_avoids_what_is_shown() {
        let area = Rect::new(0, 0, 1000, 800);
        let first = place(PlacementPolicy::Smart, (400, 300), area, &[], (0, 0), 30);
        assert_eq!(first, Rect::new(0, 0, 400, 300));
        let second = place(PlacementPolicy::Smart, (400, 300), area, &[first], (0, 0), 30);
        assert_eq!(second.intersection(&first).area(), 0);
        assert_eq!(second, Rect::new(400, 0, 400, 300));
        let big = place(
            PlacementPolicy::Smart,
            (900, 700),
            area,
            &[first, second],
            (0, 0),
            30,
        );
        assert!(big.right() <= 1000 && big.bottom() <= 800);
        assert_eq!(
            clamp(Rect::new(900, 700, 400, 300), area),
            Rect::new(600, 500, 400, 300)
        );
        assert_eq!(
            clamp(Rect::new(-50, -50, 2000, 300), area),
            Rect::new(0, 0, 2000, 300)
        );
    }

    /// Cascade steps past the last frame and starts over at the corner when the step leaves the
    /// area; center and under-mouse land where they say.
    #[test]
    fn the_other_policies_land_where_they_say() {
        let area = Rect::new(0, 0, 1000, 800);
        let last = Rect::new(100, 100, 400, 300);
        assert_eq!(
            place(PlacementPolicy::Cascade, (400, 300), area, &[last], (0, 0), 30),
            Rect::new(130, 130, 400, 300)
        );
        assert_eq!(
            place(
                PlacementPolicy::Cascade,
                (400, 300),
                area,
                &[Rect::new(590, 490, 400, 300)],
                (0, 0),
                30
            ),
            Rect::new(0, 0, 400, 300)
        );
        assert_eq!(
            place(PlacementPolicy::Cascade, (400, 300), area, &[], (0, 0), 30),
            Rect::new(30, 30, 400, 300)
        );
        assert_eq!(
            place(PlacementPolicy::Center, (400, 300), area, &[], (0, 0), 30),
            Rect::new(300, 250, 400, 300)
        );
        assert_eq!(
            place(PlacementPolicy::UnderMouse, (400, 300), area, &[], (900, 100), 30),
            Rect::new(600, 0, 400, 300)
        );
    }
}
