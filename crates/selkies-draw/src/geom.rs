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
        px >= self.x as f64 && px < self.right() as f64 && py >= self.y as f64 && py < self.bottom() as f64
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

#[cfg(test)]
mod tests {
    use super::Rect;

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
