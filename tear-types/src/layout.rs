//! Binary-tree layout for a window's panes.
//!
//! Every [`crate::TearWindow`] holds exactly one [`LayoutNode`] —
//! either a leaf (a single pane) or an internal node that splits the
//! window into two children with a given orientation. The recursive
//! structure mirrors how operators reason about tmux/screen panes:
//! "split this pane to my right, then split the bottom-half down".
//!
//! mado's existing pane.rs/tab.rs uses a flat Vec<PaneRect>; the
//! eventual M5 rebase swaps that for [`LayoutNode`] so a single
//! algorithm computes pixel rects for both apps.

use serde::{Deserialize, Serialize};

use crate::{
    direction::{Direction, SplitOrientation},
    geometry::Rect,
    id::PaneId,
};

/// Minimum fraction a side of a split may hold — keeps a resize from
/// collapsing a pane to nothing while still letting the *cell*
/// arithmetic squeeze it (a 2-cell window splits 1/1 regardless).
pub const MIN_RATIO: f32 = 0.05;

/// Outcome of [`LayoutNode::remove_leaf`]. Removing a pane either
/// collapses its parent split into the surviving sibling, removes the
/// only pane (leaving nothing the tree can represent — the caller must
/// close the window), or finds no such pane.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum LeafRemoval {
    /// The leaf was removed and its parent split collapsed into the
    /// sibling subtree. The tree still holds ≥1 pane.
    Removed,
    /// The leaf was the *root* — the tree is a single pane and cannot
    /// represent emptiness. The caller closes the window/pane instead.
    WasRoot,
    /// No leaf with that id exists in the tree.
    NotFound,
}

/// Why a [`LayoutNode`] failed [`LayoutNode::validate`]. Every variant
/// names a structurally-illegal tree so the invariant violation is a
/// typed value, not a silent mis-render (UNREPRESENTABILITY: a tree
/// that fails to validate never reaches the renderer).
// No `Eq` — `BadRatio(f32)` carries a float (NaN breaks total equality);
// `PartialEq` is enough for tests and consumers.
#[derive(Clone, Debug, PartialEq)]
pub enum LayoutError {
    /// A leaf carries [`PaneId::NULL`] — a dangling reference left by a
    /// botched removal. The renderer would draw a blank hole.
    NullLeaf,
    /// The same [`PaneId`] appears in two leaves. A pane belongs to
    /// exactly one slot; aliasing means two views fight over one PTY.
    DuplicatePane(PaneId),
    /// A split's ratio is not in the open interval `(0.0, 1.0)` — a 0 or
    /// 1 (or NaN/out-of-range) ratio means one side is unrepresentable.
    BadRatio(f32),
}

/// One node in a window's layout tree.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum LayoutNode {
    /// A single pane filling its bounding box.
    Leaf {
        pane: PaneId,
    },
    /// A split that divides its area between [`LayoutNode::Split::a`]
    /// (top/left) and [`LayoutNode::Split::b`] (bottom/right). `ratio`
    /// is a in 0.0..=1.0 — the fraction of the parent area allotted
    /// to side `a`.
    Split {
        orientation: SplitOrientation,
        ratio: f32,
        a: Box<LayoutNode>,
        b: Box<LayoutNode>,
    },
}

impl LayoutNode {
    /// Convenience constructor for a leaf.
    #[must_use]
    pub fn leaf(pane: PaneId) -> Self {
        Self::Leaf { pane }
    }

    /// Convenience constructor for a balanced (`ratio = 0.5`) split.
    #[must_use]
    pub fn split(orientation: SplitOrientation, a: LayoutNode, b: LayoutNode) -> Self {
        Self::Split {
            orientation,
            ratio: 0.5,
            a: Box::new(a),
            b: Box::new(b),
        }
    }

    /// Collect every pane id reachable from this node, in left-to-
    /// right (then top-to-bottom) traversal order. Useful for
    /// rendering a status bar that wants "the active window's panes
    /// in display order".
    #[must_use]
    pub fn panes(&self) -> Vec<PaneId> {
        let mut out = Vec::new();
        self.collect(&mut out);
        out
    }

    fn collect(&self, out: &mut Vec<PaneId>) {
        match self {
            Self::Leaf { pane } => out.push(*pane),
            Self::Split { a, b, .. } => {
                a.collect(out);
                b.collect(out);
            }
        }
    }

    /// Number of leaves in the tree (= number of panes).
    #[must_use]
    pub fn pane_count(&self) -> usize {
        match self {
            Self::Leaf { .. } => 1,
            Self::Split { a, b, .. } => a.pane_count() + b.pane_count(),
        }
    }

    /// True when this node is exactly a leaf holding `pane`.
    #[must_use]
    fn is_leaf_of(&self, pane: PaneId) -> bool {
        matches!(self, Self::Leaf { pane: p } if *p == pane)
    }

    /// True when `pane` is one of this subtree's leaves.
    #[must_use]
    pub fn contains_pane(&self, pane: PaneId) -> bool {
        match self {
            Self::Leaf { pane: p } => *p == pane,
            Self::Split { a, b, .. } => a.contains_pane(pane) || b.contains_pane(pane),
        }
    }

    /// Replace the leaf holding `target` with a split whose two children
    /// are the original pane and a fresh `new_pane`, ordered by
    /// `direction`. This is the *correct* split — it replaces only the
    /// matched leaf, leaving every other pane's position untouched
    /// (unlike a whole-window re-wrap). `origin_ratio` is the fraction
    /// the *target* keeps (clamped to `[MIN_RATIO, 1 - MIN_RATIO]`);
    /// pass `0.5` for a balanced split.
    ///
    /// Returns `true` if `target` was found and split. A `Right`/`Below`
    /// split puts the new pane after the target (right/below); a
    /// `Left`/`Above` split puts it before.
    pub fn split_leaf(
        &mut self,
        target: PaneId,
        new_pane: PaneId,
        direction: Direction,
        origin_ratio: f32,
    ) -> bool {
        match self {
            Self::Leaf { pane } if *pane == target => {
                let origin = Self::leaf(*pane);
                let fresh = Self::leaf(new_pane);
                let keep = origin_ratio.clamp(MIN_RATIO, 1.0 - MIN_RATIO);
                let orientation = direction.orientation();
                // The new pane lands after the origin for Right/Below,
                // before it for Left/Above. `ratio` is always the
                // fraction held by side `a` (top/left), so it flips
                // depending on which side the origin ends up on.
                let (a, b, ratio) = match direction {
                    Direction::Right | Direction::Below => (origin, fresh, keep),
                    Direction::Left | Direction::Above => (fresh, origin, 1.0 - keep),
                };
                *self = Self::Split {
                    orientation,
                    ratio,
                    a: Box::new(a),
                    b: Box::new(b),
                };
                true
            }
            Self::Leaf { .. } => false,
            Self::Split { a, b, .. } => {
                a.split_leaf(target, new_pane, direction, origin_ratio)
                    || b.split_leaf(target, new_pane, direction, origin_ratio)
            }
        }
    }

    /// Remove the leaf holding `target` and collapse its parent split
    /// into the surviving sibling — no dangling `NULL` leaf, no blank
    /// hole. See [`LeafRemoval`] for the three outcomes.
    pub fn remove_leaf(&mut self, target: PaneId) -> LeafRemoval {
        match self {
            Self::Leaf { pane } if *pane == target => LeafRemoval::WasRoot,
            Self::Leaf { .. } => LeafRemoval::NotFound,
            Self::Split { a, b, .. } => {
                // A direct child that *is* the target leaf collapses this
                // split into the other child.
                if a.is_leaf_of(target) {
                    *self = std::mem::replace(b.as_mut(), Self::leaf(PaneId::NULL));
                    return LeafRemoval::Removed;
                }
                if b.is_leaf_of(target) {
                    *self = std::mem::replace(a.as_mut(), Self::leaf(PaneId::NULL));
                    return LeafRemoval::Removed;
                }
                // Otherwise recurse — a deeper split collapses in place.
                match a.remove_leaf(target) {
                    LeafRemoval::NotFound => b.remove_leaf(target),
                    other => other,
                }
            }
        }
    }

    /// Move the divider of the split governing `target` along `direction`
    /// by `delta_frac` (a fraction of that split). Finds the deepest
    /// ancestor split whose orientation matches the direction's axis and
    /// contains `target`, then slides its divider: Right/Below raise the
    /// ratio (the upper/left side grows), Left/Above lower it. Returns
    /// `true` if such a divider was found.
    ///
    /// For the natural gesture — grow a left pane Right, a right pane
    /// Left, a top pane Below, a bottom pane Above — this enlarges the
    /// focused pane, because `direction` then points across the shared
    /// divider toward `target`'s neighbour. The ratio is clamped to
    /// `[MIN_RATIO, 1 - MIN_RATIO]`, so a divider never pins a pane to
    /// zero (cell rounding may still squeeze a too-small window).
    pub fn resize_leaf(&mut self, target: PaneId, direction: Direction, delta_frac: f32) -> bool {
        let want = direction.orientation();
        // Two-pass to satisfy the borrow checker: locate the governing
        // split's path (shared borrow), then mutate its ratio (exclusive
        // borrow). The side `target` descends into doesn't affect the
        // sign — `direction` alone says which way the divider slides.
        let Some((path, _target_in_a)) = self.governing_split_path(target, want) else {
            return false;
        };
        let Some(Self::Split { ratio, .. }) = self.split_at_path(&path) else {
            return false;
        };
        // Right/Below slide the divider toward side `b`, enlarging side
        // `a` (ratio up). Left/Above slide it toward `a`, enlarging `b`.
        let sign = if matches!(direction, Direction::Right | Direction::Below) {
            1.0
        } else {
            -1.0
        };
        *ratio = (*ratio + sign * delta_frac).clamp(MIN_RATIO, 1.0 - MIN_RATIO);
        true
    }

    /// Path (sequence of sides) to the deepest split of orientation
    /// `want` that has `target` in its subtree, plus whether `target`
    /// descends into that split's side `a`. `true` in a path step means
    /// "into side a".
    fn governing_split_path(
        &self,
        target: PaneId,
        want: SplitOrientation,
    ) -> Option<(Vec<bool>, bool)> {
        let mut best: Option<(Vec<bool>, bool)> = None;
        let mut path: Vec<bool> = Vec::new();
        self.walk_governing(target, want, &mut path, &mut best);
        best
    }

    fn walk_governing(
        &self,
        target: PaneId,
        want: SplitOrientation,
        path: &mut Vec<bool>,
        best: &mut Option<(Vec<bool>, bool)>,
    ) {
        if let Self::Split {
            orientation, a, b, ..
        } = self
        {
            let in_a = a.contains_pane(target);
            let in_b = b.contains_pane(target);
            if (in_a || in_b) && *orientation == want {
                // This split governs target along the wanted axis; record
                // it (deeper records overwrite shallower).
                *best = Some((path.clone(), in_a));
            }
            if in_a {
                path.push(true);
                a.walk_governing(target, want, path, best);
                path.pop();
            } else if in_b {
                path.push(false);
                b.walk_governing(target, want, path, best);
                path.pop();
            }
        }
    }

    fn split_at_path(&mut self, path: &[bool]) -> Option<&mut Self> {
        let mut node = self;
        for &into_a in path {
            match node {
                Self::Split { a, b, .. } => {
                    node = if into_a { a.as_mut() } else { b.as_mut() };
                }
                Self::Leaf { .. } => return None,
            }
        }
        Some(node)
    }

    /// Compute the pixel/cell rectangle of every pane, laying the tree
    /// out within `bounds`. This is *the* layout renderer — mado draws
    /// from it, tear-core sizes PTYs from it. Division is gap-free and
    /// overlap-free: side `b` gets exactly the remainder side `a` left.
    #[must_use]
    pub fn compute_rects(&self, bounds: Rect) -> Vec<(PaneId, Rect)> {
        let mut out = Vec::with_capacity(self.pane_count());
        self.lay_out(bounds, &mut out);
        out
    }

    fn lay_out(&self, bounds: Rect, out: &mut Vec<(PaneId, Rect)>) {
        match self {
            Self::Leaf { pane } => out.push((*pane, bounds)),
            Self::Split {
                orientation,
                ratio,
                a,
                b,
            } => {
                let (ra, rb) = split_rect(bounds, *orientation, *ratio);
                a.lay_out(ra, out);
                b.lay_out(rb, out);
            }
        }
    }

    /// The pane the operator would land on by moving `direction` from
    /// `target`, laid out within `bounds` (tmux `select-pane -L/-R/-U/-D`).
    /// Among every pane on that side that shares perpendicular edge with
    /// `target`, picks the **nearest** (smallest gap along the axis of
    /// motion), tie-broken by the **largest** shared edge. The result is
    /// independent of tree shape and traversal order. Returns `None` at
    /// the window edge or for an unknown `target`.
    #[must_use]
    pub fn neighbor(&self, target: PaneId, direction: Direction, bounds: Rect) -> Option<PaneId> {
        let rects = self.compute_rects(bounds);
        let me = rects.iter().find(|(p, _)| *p == target).map(|(_, r)| *r)?;
        // Score each candidate as (gap, overlap): a candidate is better
        // when it is closer (smaller gap) or — at equal gap — shares more
        // edge (larger overlap). `best` holds the winning (gap, overlap).
        let mut best: Option<(PaneId, u32, u32)> = None;
        for (pane, r) in &rects {
            if *pane == target {
                continue;
            }
            // (on the correct side, gap along the axis, perpendicular
            // overlap). `gap` is the clearance between the two edges
            // facing each other; the immediate neighbour of a gap-free
            // tiling has gap 0.
            let (on_side, gap, overlap) = match direction {
                Direction::Left => (
                    r.right() <= u32::from(me.x),
                    u32::from(me.x).saturating_sub(r.right()),
                    Rect::span_overlap(u32::from(r.y), r.bottom(), u32::from(me.y), me.bottom()),
                ),
                Direction::Right => (
                    u32::from(r.x) >= me.right(),
                    u32::from(r.x).saturating_sub(me.right()),
                    Rect::span_overlap(u32::from(r.y), r.bottom(), u32::from(me.y), me.bottom()),
                ),
                Direction::Above => (
                    r.bottom() <= u32::from(me.y),
                    u32::from(me.y).saturating_sub(r.bottom()),
                    Rect::span_overlap(u32::from(r.x), r.right(), u32::from(me.x), me.right()),
                ),
                Direction::Below => (
                    u32::from(r.y) >= me.bottom(),
                    u32::from(r.y).saturating_sub(me.bottom()),
                    Rect::span_overlap(u32::from(r.x), r.right(), u32::from(me.x), me.right()),
                ),
            };
            if on_side && overlap > 0 {
                let better = match best {
                    None => true,
                    Some((_, best_gap, best_overlap)) => {
                        gap < best_gap || (gap == best_gap && overlap > best_overlap)
                    }
                };
                if better {
                    best = Some((*pane, gap, overlap));
                }
            }
        }
        best.map(|(p, _, _)| p)
    }

    /// Verify the tree's structural invariants. A tree that validates is
    /// safe to render; one that fails carries a typed [`LayoutError`]
    /// naming the illegal shape instead of silently mis-drawing.
    pub fn validate(&self) -> Result<(), LayoutError> {
        let mut seen = Vec::new();
        self.validate_into(&mut seen)
    }

    fn validate_into(&self, seen: &mut Vec<PaneId>) -> Result<(), LayoutError> {
        match self {
            Self::Leaf { pane } => {
                if *pane == PaneId::NULL {
                    return Err(LayoutError::NullLeaf);
                }
                if seen.contains(pane) {
                    return Err(LayoutError::DuplicatePane(*pane));
                }
                seen.push(*pane);
                Ok(())
            }
            Self::Split { ratio, a, b, .. } => {
                if !(*ratio > 0.0 && *ratio < 1.0) {
                    return Err(LayoutError::BadRatio(*ratio));
                }
                a.validate_into(seen)?;
                b.validate_into(seen)
            }
        }
    }
}

/// Divide `bounds` between a split's two children. Horizontal splits
/// stack rows (divide the height); vertical splits sit side by side
/// (divide the width). Side `a` gets a clamped extent, side `b` the
/// exact remainder — so the two rects tile `bounds` with no gap and no
/// overlap.
fn split_rect(bounds: Rect, orientation: SplitOrientation, ratio: f32) -> (Rect, Rect) {
    match orientation {
        SplitOrientation::Horizontal => {
            let a_h = split_extent(bounds.h, ratio);
            let b_h = bounds.h - a_h;
            (
                Rect::new(bounds.x, bounds.y, bounds.w, a_h),
                Rect::new(bounds.x, bounds.y + a_h, bounds.w, b_h),
            )
        }
        SplitOrientation::Vertical => {
            let a_w = split_extent(bounds.w, ratio);
            let b_w = bounds.w - a_w;
            (
                Rect::new(bounds.x, bounds.y, a_w, bounds.h),
                Rect::new(bounds.x + a_w, bounds.y, b_w, bounds.h),
            )
        }
    }
}

/// The number of cells side `a` gets from `total` at `ratio`. Clamped so
/// both sides keep ≥1 cell whenever `total >= 2`; a 1-cell total can't be
/// divided (side `a` takes it, `b` gets 0) and a 0-cell total yields 0.
fn split_extent(total: u16, ratio: f32) -> u16 {
    if total <= 1 {
        return total;
    }
    let raw = (f32::from(total) * ratio).round();
    // round() of a finite product in [0, total] is in range; clamp pins
    // NaN-free bounds and guarantees neither side vanishes.
    let a = raw.clamp(1.0, f32::from(total) - 1.0);
    a as u16
}

/// Named tmux-style layout presets. tmux ships five built-ins; tear
/// supports the same plus a `tatami`-style auto-balance.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum LayoutKind {
    /// All panes in one horizontal row.
    EvenHorizontal,
    /// All panes in one vertical column.
    EvenVertical,
    /// One large pane on top, all others on the bottom row.
    MainHorizontal,
    /// One large pane on the left, all others stacked on the right.
    MainVertical,
    /// Tiled: arrange panes in an approximate square grid.
    Tiled,
    /// Custom: the [`LayoutNode`] tree on the [`crate::TearWindow`] is
    /// the source of truth. Operators reach this state after manual
    /// splits / resizes; tear-core serialises the tree as the canonical
    /// shape.
    Custom,
}

/// Size specification for a pane within a layout.
#[derive(Copy, Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Size {
    /// Fixed number of terminal cells.
    Cells(u16),
    /// Fraction of the parent's available space (`0.0..=1.0`).
    Fraction(f32),
    /// Automatic — the parent layout decides.
    Auto,
}

impl Default for Size {
    fn default() -> Self {
        Self::Auto
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::direction::SplitOrientation;

    #[test]
    fn leaf_has_one_pane() {
        let n = LayoutNode::leaf(PaneId(7));
        assert_eq!(n.pane_count(), 1);
        assert_eq!(n.panes(), vec![PaneId(7)]);
    }

    #[test]
    fn split_aggregates_panes_left_then_right() {
        let n = LayoutNode::split(
            SplitOrientation::Vertical,
            LayoutNode::leaf(PaneId(1)),
            LayoutNode::leaf(PaneId(2)),
        );
        assert_eq!(n.pane_count(), 2);
        assert_eq!(n.panes(), vec![PaneId(1), PaneId(2)]);
    }

    #[test]
    fn nested_split_traversal_is_predictable() {
        let n = LayoutNode::split(
            SplitOrientation::Horizontal,
            LayoutNode::leaf(PaneId(1)),
            LayoutNode::split(
                SplitOrientation::Vertical,
                LayoutNode::leaf(PaneId(2)),
                LayoutNode::leaf(PaneId(3)),
            ),
        );
        assert_eq!(n.panes(), vec![PaneId(1), PaneId(2), PaneId(3)]);
        assert_eq!(n.pane_count(), 3);
    }

    // ── split_leaf ───────────────────────────────────────────────

    #[test]
    fn split_leaf_targets_the_matched_leaf_only() {
        // Tree: 1 | (2 / 3). Splitting pane 2 to the right must touch
        // ONLY pane 2's slot — panes 1 and 3 keep their positions. This
        // is the bug the old whole-window re-wrap had.
        let mut n = LayoutNode::split(
            SplitOrientation::Vertical,
            LayoutNode::leaf(PaneId(1)),
            LayoutNode::split(
                SplitOrientation::Horizontal,
                LayoutNode::leaf(PaneId(2)),
                LayoutNode::leaf(PaneId(3)),
            ),
        );
        assert!(n.split_leaf(PaneId(2), PaneId(9), Direction::Right, 0.5));
        // Display order: 1, then (2-split-9), then 3.
        assert_eq!(n.panes(), vec![PaneId(1), PaneId(2), PaneId(9), PaneId(3)]);
        assert_eq!(n.pane_count(), 4);
        n.validate().unwrap();
    }

    #[test]
    fn split_leaf_orders_new_pane_by_direction() {
        let mut right = LayoutNode::leaf(PaneId(1));
        right.split_leaf(PaneId(1), PaneId(2), Direction::Right, 0.5);
        assert_eq!(right.panes(), vec![PaneId(1), PaneId(2)]); // origin first

        let mut left = LayoutNode::leaf(PaneId(1));
        left.split_leaf(PaneId(1), PaneId(2), Direction::Left, 0.5);
        assert_eq!(left.panes(), vec![PaneId(2), PaneId(1)]); // new first
    }

    #[test]
    fn split_leaf_unknown_target_is_noop() {
        let mut n = LayoutNode::leaf(PaneId(1));
        assert!(!n.split_leaf(PaneId(99), PaneId(2), Direction::Right, 0.5));
        assert_eq!(n.panes(), vec![PaneId(1)]);
    }

    #[test]
    fn split_leaf_clamps_extreme_ratio() {
        let mut n = LayoutNode::leaf(PaneId(1));
        n.split_leaf(PaneId(1), PaneId(2), Direction::Right, 0.0);
        // 0.0 origin-keep clamps to MIN_RATIO — still a valid split.
        n.validate().unwrap();
        if let LayoutNode::Split { ratio, .. } = n {
            assert!(ratio >= MIN_RATIO && ratio <= 1.0 - MIN_RATIO);
        } else {
            panic!("expected a split");
        }
    }

    // ── remove_leaf ──────────────────────────────────────────────

    #[test]
    fn remove_leaf_collapses_parent_into_sibling() {
        // (1 | 2) → remove 1 → just 2 (no dangling NULL).
        let mut n = LayoutNode::split(
            SplitOrientation::Vertical,
            LayoutNode::leaf(PaneId(1)),
            LayoutNode::leaf(PaneId(2)),
        );
        assert_eq!(n.remove_leaf(PaneId(1)), LeafRemoval::Removed);
        assert_eq!(n, LayoutNode::leaf(PaneId(2)));
        n.validate().unwrap();
    }

    #[test]
    fn remove_leaf_collapses_deep_node() {
        // 1 | (2 / 3) → remove 3 → 1 | 2.
        let mut n = LayoutNode::split(
            SplitOrientation::Vertical,
            LayoutNode::leaf(PaneId(1)),
            LayoutNode::split(
                SplitOrientation::Horizontal,
                LayoutNode::leaf(PaneId(2)),
                LayoutNode::leaf(PaneId(3)),
            ),
        );
        assert_eq!(n.remove_leaf(PaneId(3)), LeafRemoval::Removed);
        assert_eq!(n.panes(), vec![PaneId(1), PaneId(2)]);
        assert!(!n.contains_pane(PaneId(3)));
        // No PaneId::NULL leaf left behind.
        n.validate().unwrap();
    }

    #[test]
    fn remove_leaf_root_reports_was_root() {
        let mut n = LayoutNode::leaf(PaneId(1));
        assert_eq!(n.remove_leaf(PaneId(1)), LeafRemoval::WasRoot);
        // Tree untouched — caller closes the window instead.
        assert_eq!(n, LayoutNode::leaf(PaneId(1)));
    }

    #[test]
    fn remove_leaf_unknown_is_not_found() {
        let mut n = LayoutNode::split(
            SplitOrientation::Vertical,
            LayoutNode::leaf(PaneId(1)),
            LayoutNode::leaf(PaneId(2)),
        );
        assert_eq!(n.remove_leaf(PaneId(99)), LeafRemoval::NotFound);
        assert_eq!(n.pane_count(), 2);
    }

    // ── compute_rects ────────────────────────────────────────────

    #[test]
    fn compute_rects_single_pane_fills_bounds() {
        let n = LayoutNode::leaf(PaneId(1));
        let r = n.compute_rects(Rect::sized(80, 24));
        assert_eq!(r, vec![(PaneId(1), Rect::new(0, 0, 80, 24))]);
    }

    #[test]
    fn compute_rects_vertical_split_is_side_by_side_gapless() {
        // Vertical split divides width; a left, b right; no gap/overlap.
        let n = LayoutNode::split(
            SplitOrientation::Vertical,
            LayoutNode::leaf(PaneId(1)),
            LayoutNode::leaf(PaneId(2)),
        );
        let r = n.compute_rects(Rect::sized(80, 24));
        let a = r.iter().find(|(p, _)| *p == PaneId(1)).unwrap().1;
        let b = r.iter().find(|(p, _)| *p == PaneId(2)).unwrap().1;
        assert_eq!(a, Rect::new(0, 0, 40, 24));
        assert_eq!(b, Rect::new(40, 0, 40, 24));
        assert_eq!(a.right(), u32::from(b.x)); // gapless
    }

    #[test]
    fn compute_rects_horizontal_split_stacks_rows() {
        let n = LayoutNode::split(
            SplitOrientation::Horizontal,
            LayoutNode::leaf(PaneId(1)),
            LayoutNode::leaf(PaneId(2)),
        );
        let r = n.compute_rects(Rect::sized(80, 24));
        let a = r.iter().find(|(p, _)| *p == PaneId(1)).unwrap().1;
        let b = r.iter().find(|(p, _)| *p == PaneId(2)).unwrap().1;
        assert_eq!(a, Rect::new(0, 0, 80, 12));
        assert_eq!(b, Rect::new(0, 12, 80, 12));
        assert_eq!(a.bottom(), u32::from(b.y));
    }

    #[test]
    fn compute_rects_tiles_bounds_exactly() {
        // The union of all leaf rects must equal bounds.area() with no
        // double-counting — proves gap-free + overlap-free for a nested
        // tree at an odd size (forces rounding).
        let n = LayoutNode::split(
            SplitOrientation::Vertical,
            LayoutNode::leaf(PaneId(1)),
            LayoutNode::split(
                SplitOrientation::Horizontal,
                LayoutNode::leaf(PaneId(2)),
                LayoutNode::leaf(PaneId(3)),
            ),
        );
        let bounds = Rect::sized(81, 25);
        let rects = n.compute_rects(bounds);
        let total: u32 = rects.iter().map(|(_, r)| r.area()).sum();
        assert_eq!(total, bounds.area());
        // No overlap: every interior cell belongs to exactly one rect.
        for y in 0..bounds.h {
            for x in 0..bounds.w {
                let owners = rects.iter().filter(|(_, r)| r.contains(x, y)).count();
                assert_eq!(owners, 1, "cell ({x},{y}) owned by {owners} panes");
            }
        }
    }

    #[test]
    fn compute_rects_tiny_window_never_panics() {
        let n = LayoutNode::split(
            SplitOrientation::Vertical,
            LayoutNode::leaf(PaneId(1)),
            LayoutNode::leaf(PaneId(2)),
        );
        // 1-cell width can't show two panes side-by-side; b is squeezed
        // to 0 but the call is total and gapless.
        let r = n.compute_rects(Rect::sized(1, 5));
        let total: u32 = r.iter().map(|(_, rr)| rr.area()).sum();
        assert_eq!(total, 5);
    }

    // ── neighbor ─────────────────────────────────────────────────

    #[test]
    fn neighbor_walks_left_and_right() {
        // 1 | 2 | 3 (three columns).
        let n = LayoutNode::split(
            SplitOrientation::Vertical,
            LayoutNode::leaf(PaneId(1)),
            LayoutNode::split(
                SplitOrientation::Vertical,
                LayoutNode::leaf(PaneId(2)),
                LayoutNode::leaf(PaneId(3)),
            ),
        );
        let b = Rect::sized(90, 24);
        assert_eq!(n.neighbor(PaneId(2), Direction::Left, b), Some(PaneId(1)));
        assert_eq!(n.neighbor(PaneId(2), Direction::Right, b), Some(PaneId(3)));
        assert_eq!(n.neighbor(PaneId(1), Direction::Left, b), None); // window edge
        assert_eq!(n.neighbor(PaneId(3), Direction::Right, b), None);
    }

    #[test]
    fn neighbor_crosses_split_boundary_by_edge_overlap() {
        // Left column = pane 1 (full height). Right column split into
        // 2 (top) / 3 (bottom). From 1 going Right, the neighbour with
        // the most shared edge is whichever covers the focal band; both
        // 2 and 3 are valid candidates — assert it returns one of them.
        let n = LayoutNode::split(
            SplitOrientation::Vertical,
            LayoutNode::leaf(PaneId(1)),
            LayoutNode::split(
                SplitOrientation::Horizontal,
                LayoutNode::leaf(PaneId(2)),
                LayoutNode::leaf(PaneId(3)),
            ),
        );
        let b = Rect::sized(80, 24);
        let right = n.neighbor(PaneId(1), Direction::Right, b);
        assert!(matches!(right, Some(PaneId(2)) | Some(PaneId(3))));
        // And from 2 going Left lands back on 1.
        assert_eq!(n.neighbor(PaneId(2), Direction::Left, b), Some(PaneId(1)));
    }

    #[test]
    fn neighbor_unknown_target_is_none() {
        let n = LayoutNode::leaf(PaneId(1));
        assert_eq!(n.neighbor(PaneId(99), Direction::Left, Rect::sized(80, 24)), None);
    }

    #[test]
    fn neighbor_prefers_nearest_not_just_max_overlap() {
        // 1 | 2 | 3 — from 1, both 2 and 3 are to the Right with equal
        // (full-height) overlap. The NEAREST (2) must win regardless of
        // how the tree nests 2 and 3 (right-leaning vs left-leaning).
        let right_leaning = LayoutNode::split(
            SplitOrientation::Vertical,
            LayoutNode::leaf(PaneId(1)),
            LayoutNode::split(
                SplitOrientation::Vertical,
                LayoutNode::leaf(PaneId(2)),
                LayoutNode::leaf(PaneId(3)),
            ),
        );
        let left_leaning = LayoutNode::split(
            SplitOrientation::Vertical,
            LayoutNode::split(
                SplitOrientation::Vertical,
                LayoutNode::leaf(PaneId(1)),
                LayoutNode::leaf(PaneId(2)),
            ),
            LayoutNode::leaf(PaneId(3)),
        );
        let b = Rect::sized(90, 24);
        assert_eq!(right_leaning.neighbor(PaneId(1), Direction::Right, b), Some(PaneId(2)));
        assert_eq!(left_leaning.neighbor(PaneId(1), Direction::Right, b), Some(PaneId(2)));
    }

    /// A pseudo-property test: across a spread of tree shapes and bounds
    /// (including odd sizes that force rounding), `compute_rects` must
    /// tile `bounds` exactly — total area equals `bounds.area()` and no
    /// interior cell is owned by two panes. This is the load-bearing
    /// invariant both apps depend on (no seams, no double-draws).
    #[test]
    fn compute_rects_tiles_exactly_across_shapes_and_sizes() {
        let shapes = [
            LayoutNode::leaf(PaneId(1)),
            LayoutNode::split(
                SplitOrientation::Vertical,
                LayoutNode::leaf(PaneId(1)),
                LayoutNode::leaf(PaneId(2)),
            ),
            LayoutNode::split(
                SplitOrientation::Horizontal,
                LayoutNode::split(
                    SplitOrientation::Vertical,
                    LayoutNode::leaf(PaneId(1)),
                    LayoutNode::leaf(PaneId(2)),
                ),
                LayoutNode::split(
                    SplitOrientation::Vertical,
                    LayoutNode::leaf(PaneId(3)),
                    LayoutNode::split(
                        SplitOrientation::Horizontal,
                        LayoutNode::leaf(PaneId(4)),
                        LayoutNode::leaf(PaneId(5)),
                    ),
                ),
            ),
        ];
        for shape in &shapes {
            for &(w, h) in &[(80u16, 24u16), (81, 25), (1, 1), (3, 200), (200, 3), (2, 2)] {
                let bounds = Rect::sized(w, h);
                let rects = shape.compute_rects(bounds);
                let total: u32 = rects.iter().map(|(_, r)| r.area()).sum();
                assert_eq!(total, bounds.area(), "area mismatch at {w}x{h}");
                // Every leaf appears exactly once.
                assert_eq!(rects.len(), shape.pane_count());
                // No interior cell double-owned (sample the full grid for
                // small bounds; the area equality covers the rest).
                if bounds.area() <= 8192 {
                    for y in 0..h {
                        for x in 0..w {
                            let owners = rects.iter().filter(|(_, r)| r.contains(x, y)).count();
                            assert!(owners <= 1, "cell ({x},{y}) owned by {owners} at {w}x{h}");
                        }
                    }
                }
            }
        }
    }

    // ── resize_leaf ──────────────────────────────────────────────

    #[test]
    fn resize_leaf_grows_focused_pane_rightward() {
        // 1 | 2, vertical. "grow 1 to the Right" enlarges pane 1.
        let mut n = LayoutNode::split(
            SplitOrientation::Vertical,
            LayoutNode::leaf(PaneId(1)),
            LayoutNode::leaf(PaneId(2)),
        );
        let before = n.compute_rects(Rect::sized(80, 24));
        let w1_before = before.iter().find(|(p, _)| *p == PaneId(1)).unwrap().1.w;
        assert!(n.resize_leaf(PaneId(1), Direction::Right, 0.2));
        let after = n.compute_rects(Rect::sized(80, 24));
        let w1_after = after.iter().find(|(p, _)| *p == PaneId(1)).unwrap().1.w;
        assert!(w1_after > w1_before, "{w1_after} !> {w1_before}");
    }

    #[test]
    fn resize_leaf_grows_pane_on_side_b_too() {
        // 1 | 2; "grow 2 to the Left" enlarges pane 2 (side b).
        let mut n = LayoutNode::split(
            SplitOrientation::Vertical,
            LayoutNode::leaf(PaneId(1)),
            LayoutNode::leaf(PaneId(2)),
        );
        let before = n.compute_rects(Rect::sized(80, 24));
        let w2_before = before.iter().find(|(p, _)| *p == PaneId(2)).unwrap().1.w;
        assert!(n.resize_leaf(PaneId(2), Direction::Left, 0.2));
        let after = n.compute_rects(Rect::sized(80, 24));
        let w2_after = after.iter().find(|(p, _)| *p == PaneId(2)).unwrap().1.w;
        assert!(w2_after > w2_before, "{w2_after} !> {w2_before}");
    }

    #[test]
    fn resize_leaf_ignores_wrong_axis() {
        // 1 | 2 vertical; resizing Up/Down finds no horizontal split.
        let mut n = LayoutNode::split(
            SplitOrientation::Vertical,
            LayoutNode::leaf(PaneId(1)),
            LayoutNode::leaf(PaneId(2)),
        );
        assert!(!n.resize_leaf(PaneId(1), Direction::Below, 0.2));
    }

    #[test]
    fn resize_leaf_picks_deepest_governing_split() {
        // (1 | 2) stacked-over 3 — both a vertical and an enclosing
        // horizontal split exist. Resizing 1 to the Right must adjust
        // the inner VERTICAL split (1|2), not the outer horizontal one.
        let mut n = LayoutNode::split(
            SplitOrientation::Horizontal,
            LayoutNode::split(
                SplitOrientation::Vertical,
                LayoutNode::leaf(PaneId(1)),
                LayoutNode::leaf(PaneId(2)),
            ),
            LayoutNode::leaf(PaneId(3)),
        );
        let bounds = Rect::sized(80, 24);
        let w1_before = n.compute_rects(bounds).iter().find(|(p, _)| *p == PaneId(1)).unwrap().1.w;
        let h3_before = n.compute_rects(bounds).iter().find(|(p, _)| *p == PaneId(3)).unwrap().1.h;
        assert!(n.resize_leaf(PaneId(1), Direction::Right, 0.2));
        let after = n.compute_rects(bounds);
        let w1_after = after.iter().find(|(p, _)| *p == PaneId(1)).unwrap().1.w;
        let h3_after = after.iter().find(|(p, _)| *p == PaneId(3)).unwrap().1.h;
        assert!(w1_after > w1_before); // inner split moved
        assert_eq!(h3_after, h3_before); // outer split untouched
    }

    // ── validate ─────────────────────────────────────────────────

    #[test]
    fn validate_rejects_null_leaf() {
        let n = LayoutNode::leaf(PaneId::NULL);
        assert_eq!(n.validate(), Err(LayoutError::NullLeaf));
    }

    #[test]
    fn validate_rejects_duplicate_pane() {
        let n = LayoutNode::split(
            SplitOrientation::Vertical,
            LayoutNode::leaf(PaneId(5)),
            LayoutNode::leaf(PaneId(5)),
        );
        assert_eq!(n.validate(), Err(LayoutError::DuplicatePane(PaneId(5))));
    }

    #[test]
    fn validate_rejects_degenerate_ratio() {
        let n = LayoutNode::Split {
            orientation: SplitOrientation::Vertical,
            ratio: 0.0,
            a: Box::new(LayoutNode::leaf(PaneId(1))),
            b: Box::new(LayoutNode::leaf(PaneId(2))),
        };
        assert_eq!(n.validate(), Err(LayoutError::BadRatio(0.0)));
    }

    #[test]
    fn validate_accepts_well_formed_tree() {
        let n = LayoutNode::split(
            SplitOrientation::Vertical,
            LayoutNode::leaf(PaneId(1)),
            LayoutNode::split(
                SplitOrientation::Horizontal,
                LayoutNode::leaf(PaneId(2)),
                LayoutNode::leaf(PaneId(3)),
            ),
        );
        n.validate().unwrap();
    }

    // ── round-trip: split then remove returns to the original ────

    #[test]
    fn split_then_remove_is_identity() {
        let original = LayoutNode::leaf(PaneId(1));
        let mut n = original.clone();
        n.split_leaf(PaneId(1), PaneId(2), Direction::Right, 0.5);
        assert_eq!(n.pane_count(), 2);
        assert_eq!(n.remove_leaf(PaneId(2)), LeafRemoval::Removed);
        assert_eq!(n, original);
    }
}
