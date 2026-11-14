/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! Integer rectangles in logical surface coordinates.

/// An axis-aligned rectangle in logical pixels.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Rect {
    /// Left edge.
    pub x: i32,
    /// Top edge.
    pub y: i32,
    /// Width, never negative.
    pub w: i32,
    /// Height, never negative.
    pub h: i32,
}

impl Rect {
    /// Builds a rectangle from its corner and size.
    ///
    /// # Arguments
    /// * `x` - Left edge.
    /// * `y` - Top edge.
    /// * `w` - Width; a negative value is clamped to zero.
    /// * `h` - Height; a negative value is clamped to zero.
    pub const fn new(x: i32, y: i32, w: i32, h: i32) -> Rect {
        Rect {
            x,
            y,
            w: if w < 0 { 0 } else { w },
            h: if h < 0 { 0 } else { h },
        }
    }

    /// Right edge, exclusive.
    pub const fn right(&self) -> i32 {
        self.x + self.w
    }

    /// Bottom edge, exclusive.
    pub const fn bottom(&self) -> i32 {
        self.y + self.h
    }

    /// Whether the rectangle covers no pixels.
    pub const fn is_empty(&self) -> bool {
        self.w <= 0 || self.h <= 0
    }

    /// Whether a point lies inside the rectangle.
    ///
    /// # Arguments
    /// * `px` - Point X coordinate.
    /// * `py` - Point Y coordinate.
    pub fn contains(&self, px: f64, py: f64) -> bool {
        px >= f64::from(self.x)
            && px < f64::from(self.right())
            && py >= f64::from(self.y)
            && py < f64::from(self.bottom())
    }

    /// The overlap of two rectangles, empty when they do not meet.
    ///
    /// # Arguments
    /// * `other` - The rectangle to intersect with.
    pub fn intersect(&self, other: &Rect) -> Rect {
        let x = self.x.max(other.x);
        let y = self.y.max(other.y);
        let r = self.right().min(other.right());
        let b = self.bottom().min(other.bottom());
        if r <= x || b <= y {
            return Rect::default();
        }
        Rect::new(x, y, r - x, b - y)
    }

    /// The smallest rectangle covering both.
    ///
    /// # Arguments
    /// * `other` - The rectangle to merge with.
    pub fn union(&self, other: &Rect) -> Rect {
        if self.is_empty() {
            return *other;
        }
        if other.is_empty() {
            return *self;
        }
        let x = self.x.min(other.x);
        let y = self.y.min(other.y);
        let r = self.right().max(other.right());
        let b = self.bottom().max(other.bottom());
        Rect::new(x, y, r - x, b - y)
    }

    /// The rectangle grown by the same margin on every side.
    ///
    /// # Arguments
    /// * `by` - Pixels to add on each side; negative shrinks.
    pub fn inflate(&self, by: i32) -> Rect {
        Rect::new(self.x - by, self.y - by, self.w + 2 * by, self.h + 2 * by)
    }

    /// The rectangle with every coordinate multiplied by an integer scale.
    ///
    /// # Arguments
    /// * `scale` - The output scale factor.
    pub fn scaled(&self, scale: i32) -> Rect {
        Rect::new(self.x * scale, self.y * scale, self.w * scale, self.h * scale)
    }
}

/// Merges the rectangles that overlap into the one covering both wherever it covers no more
/// pixels than the two did apart, and drops the empty ones, so a region handled rectangle by
/// rectangle handles no pixel twice: a frame's damage is the area it left and the area it
/// drew, nearly the same rectangle while something slides, and a buffer that missed several
/// frames holds every one of them.
///
/// # Arguments
/// * `rects` - The rectangles, merged in place.
pub fn coalesce(rects: &mut Vec<Rect>) {
    let area = |r: &Rect| i64::from(r.w) * i64::from(r.h);
    rects.retain(|r| !r.is_empty());
    let mut i = 0;
    while i < rects.len() {
        let mut merged = false;
        let mut j = i + 1;
        while j < rects.len() {
            let (a, b) = (rects[i], rects[j]);
            let union = a.union(&b);
            if !a.intersect(&b).is_empty() && area(&union) <= area(&a) + area(&b) {
                rects[i] = union;
                rects.swap_remove(j);
                merged = true;
            } else {
                j += 1;
            }
        }
        if !merged {
            i += 1;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{Rect, coalesce};

    /// Overlapping rectangles become the one covering them where that adds no pixels; ones
    /// that cross, whose cover would, and ones apart stay as they are.
    #[test]
    fn overlapping_damage_coalesces_without_growing() {
        let mut slide = vec![
            Rect::new(6, 1226, 2548, 48),
            Rect::new(5, 1226, 2550, 49),
            Rect::default(),
            Rect::new(6, 1226, 2548, 48),
        ];
        coalesce(&mut slide);
        assert_eq!(slide, vec![Rect::new(5, 1226, 2550, 49)]);
        let mut cross = vec![Rect::new(0, 40, 100, 10), Rect::new(40, 0, 10, 100)];
        coalesce(&mut cross);
        assert_eq!(cross.len(), 2);
        let mut apart = vec![Rect::new(0, 0, 10, 10), Rect::new(20, 0, 10, 10)];
        coalesce(&mut apart);
        assert_eq!(apart.len(), 2);
    }

    #[test]
    fn intersect_and_union() {
        let a = Rect::new(0, 0, 10, 10);
        let b = Rect::new(5, 5, 10, 10);
        assert_eq!(a.intersect(&b), Rect::new(5, 5, 5, 5));
        assert_eq!(a.union(&b), Rect::new(0, 0, 15, 15));
        assert!(a.intersect(&Rect::new(20, 20, 1, 1)).is_empty());
        assert_eq!(Rect::default().union(&b), b);
    }

    #[test]
    fn contains_is_half_open() {
        let r = Rect::new(2, 2, 4, 4);
        assert!(r.contains(2.0, 2.0));
        assert!(r.contains(5.9, 5.9));
        assert!(!r.contains(6.0, 3.0));
        assert!(!r.contains(1.9, 3.0));
    }
}
