//! Axis-aligned rectangles — the geometry the layout algebra renders to.
//!
//! A [`Rect`] is the bounding box a [`crate::LayoutNode`] subtree is
//! allotted. The unit is the *caller's*: tear-core measures in terminal
//! cells (cols × rows), mado scales the same rects to device pixels for
//! the GPU. One [`crate::LayoutNode::compute_rects`] algorithm serves
//! both — so a split looks identical whether the daemon is sizing a PTY
//! winsize or mado is drawing pane borders. That single-renderer
//! property is the whole reason this type lives in `tear-types` and not
//! in either app (the compounding directive's "solve once" rule).
//!
//! Cells are `u16` — a terminal never exceeds 65535 cols/rows, and the
//! width/height of a child is always ≤ its parent, so no arithmetic here
//! overflows. Division is *gap-free and overlap-free by construction*:
//! a split hands side `a` a clamped extent and side `b` the exact
//! remainder, so `a.right() == b.x` (or `a.bottom() == b.y`) always —
//! there is no representable layout with a one-cell seam or a one-cell
//! double-draw.

use serde::{Deserialize, Serialize};

/// An axis-aligned rectangle in a discrete cell grid. `x`/`y` are the
/// top-left origin; `w`/`h` are width/height. A zero-area rect is legal
/// (it models a pane squeezed out of a too-small window) but
/// [`Rect::is_empty`] flags it so renderers can skip it.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Rect {
    pub x: u16,
    pub y: u16,
    pub w: u16,
    pub h: u16,
}

impl Rect {
    /// A rect at the origin with the given size — the usual window-root
    /// bounding box (`Rect::new(0, 0, cols, rows)`).
    #[must_use]
    pub const fn new(x: u16, y: u16, w: u16, h: u16) -> Self {
        Self { x, y, w, h }
    }

    /// A rect rooted at the origin (`x = y = 0`).
    #[must_use]
    pub const fn sized(w: u16, h: u16) -> Self {
        Self { x: 0, y: 0, w, h }
    }

    /// The x coordinate one past the right edge (`x + w`). Widened to
    /// `u32` so a full-width rect at a high `x` cannot wrap.
    #[must_use]
    pub const fn right(self) -> u32 {
        self.x as u32 + self.w as u32
    }

    /// The y coordinate one past the bottom edge (`y + h`).
    #[must_use]
    pub const fn bottom(self) -> u32 {
        self.y as u32 + self.h as u32
    }

    /// Area in cells.
    #[must_use]
    pub const fn area(self) -> u32 {
        self.w as u32 * self.h as u32
    }

    /// True when either dimension is zero — the pane has been squeezed
    /// to nothing and a renderer should skip it.
    #[must_use]
    pub const fn is_empty(self) -> bool {
        self.w == 0 || self.h == 0
    }

    /// True when `(px, py)` falls inside the rect (right/bottom edges
    /// exclusive — adjacent rects never both claim a boundary cell).
    #[must_use]
    pub fn contains(self, px: u16, py: u16) -> bool {
        let (px, py) = (u32::from(px), u32::from(py));
        u32::from(self.x) <= px
            && px < self.right()
            && u32::from(self.y) <= py
            && py < self.bottom()
    }

    /// Length of the overlap between two 1-D intervals `[a0, a1)` and
    /// `[b0, b1)`. Used by directional navigation to score how much two
    /// panes share an edge. Returns 0 when they don't overlap.
    #[must_use]
    pub(crate) fn span_overlap(a0: u32, a1: u32, b0: u32, b1: u32) -> u32 {
        let lo = a0.max(b0);
        let hi = a1.min(b1);
        hi.saturating_sub(lo)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn right_and_bottom_are_exclusive_edges() {
        let r = Rect::new(2, 3, 10, 5);
        assert_eq!(r.right(), 12);
        assert_eq!(r.bottom(), 8);
        assert!(r.contains(2, 3)); // top-left inclusive
        assert!(!r.contains(12, 3)); // right exclusive
        assert!(!r.contains(2, 8)); // bottom exclusive
        assert!(r.contains(11, 7)); // last interior cell
    }

    #[test]
    fn empty_when_either_dim_zero() {
        assert!(Rect::sized(0, 9).is_empty());
        assert!(Rect::sized(9, 0).is_empty());
        assert!(!Rect::sized(1, 1).is_empty());
        assert_eq!(Rect::new(0, 0, 4, 5).area(), 20);
    }

    #[test]
    fn span_overlap_is_intersection_length() {
        assert_eq!(Rect::span_overlap(0, 10, 5, 15), 5);
        assert_eq!(Rect::span_overlap(0, 5, 5, 10), 0); // touching, not overlapping
        assert_eq!(Rect::span_overlap(0, 5, 10, 15), 0); // disjoint
        assert_eq!(Rect::span_overlap(2, 8, 0, 20), 6); // fully contained
    }

    #[test]
    fn high_coordinate_right_edge_does_not_wrap() {
        let r = Rect::new(u16::MAX - 3, 0, 3, 1);
        assert_eq!(r.right(), u32::from(u16::MAX));
    }
}
