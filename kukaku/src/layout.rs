//! Binary-tree layout: the split algebra, generic over what a leaf IS.
//!
//! Every the consumer's window holds exactly one [`LayoutNode`] —
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
    leaf::LeafId,
};

/// Minimum fraction a side of a split may hold — keeps a resize from
/// collapsing a pane to nothing while still letting the *cell*
/// arithmetic squeeze it (a 2-cell window splits 1/1 regardless).
pub const MIN_RATIO: f32 = 0.05;

/// The fraction of a split's area allotted to side `a`, refined so that
/// the illegal values have no representation.
///
/// ## Why a newtype and not a validated `f32`
///
/// The field used to be a public `f32`, which meant `0.0`, `1.0`, `-3.0`,
/// `inf` and `NaN` were all constructible **and all deserialisable from the
/// wire**. `validate()` rejected them — but `validate()` is called by a
/// caller who remembers to, and CBOR deserialisation writes the field
/// directly, so the guard was one forgotten call away from useless.
///
/// `NaN` was the worst of them because it fails *silently in the wrong
/// direction*. `split_extent` computes `(total * ratio).round()` and then
/// clamps — and **`f32::clamp` returns `NaN` for a `NaN` input** (it only
/// panics when the *bounds* are NaN), after which `NaN as u16` saturates to
/// `0`. So a NaN ratio did not panic and did not error: side `a` silently
/// got zero cells and the pane vanished. The old comment on `split_extent`
/// claimed the clamp "pins NaN-free bounds"; it did not.
///
/// Now the only way in is [`SplitRatio::new`], which normalises non-finite
/// input to the balanced default and clamps the rest into
/// `[MIN_RATIO, 1 - MIN_RATIO]`. The field is private, `Deserialize` routes
/// through the same constructor, and `Serialize` is transparent — so the
/// **CBOR wire shape is byte-identical to the bare `f32` it replaces.**
///
/// Tier: **truly-unrepresentable** in-process (no constructor produces an
/// out-of-range or non-finite value); **parse-time-normalised** on the wire.
#[derive(Copy, Clone, Debug, PartialEq, Serialize)]
#[serde(transparent)]
pub struct SplitRatio(f32);

impl SplitRatio {
    /// An even split. The value every balanced constructor uses.
    pub const BALANCED: Self = Self(0.5);

    /// Refine an arbitrary `f32`.
    ///
    /// Non-finite input (`NaN`, `±inf`) becomes [`Self::BALANCED`] rather
    /// than propagating: there is no sensible clamp for a value that is not
    /// on the number line, and silently yielding a zero-width pane is the
    /// bug this type exists to remove.
    #[must_use]
    pub fn new(v: f32) -> Self {
        if v.is_finite() {
            Self(v.clamp(MIN_RATIO, 1.0 - MIN_RATIO))
        } else {
            Self::BALANCED
        }
    }

    /// The refined value. Always finite and always within
    /// `[MIN_RATIO, 1 - MIN_RATIO]`.
    #[must_use]
    pub const fn get(self) -> f32 {
        self.0
    }
}

impl Default for SplitRatio {
    fn default() -> Self {
        Self::BALANCED
    }
}

impl From<f32> for SplitRatio {
    fn from(v: f32) -> Self {
        Self::new(v)
    }
}

impl<'de> Deserialize<'de> for SplitRatio {
    /// Routes through [`SplitRatio::new`], so a hostile or merely stale
    /// peer cannot put an out-of-range or `NaN` ratio into a live tree.
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        Ok(Self::new(f32::deserialize(d)?))
    }
}

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
pub enum LayoutError<Id: LeafId> {
    /// The same leaf id appears in two leaves. A leaf belongs to exactly
    /// one slot; aliasing means two views fight over one thing.
    DuplicatePane(Id),
    /// A split's ratio is not in the open interval `(0.0, 1.0)` — a 0 or
    /// 1 (or NaN/out-of-range) ratio means one side is unrepresentable.
    BadRatio(f32),
}

/// One node in a window's layout tree.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum LayoutNode<Id: LeafId> {
    /// A single pane filling its bounding box.
    Leaf {
        pane: Id,
    },
    /// A split that divides its area between [`LayoutNode::Split::a`]
    /// (top/left) and [`LayoutNode::Split::b`] (bottom/right). `ratio`
    /// is a in 0.0..=1.0 — the fraction of the parent area allotted
    /// to side `a`.
    Split {
        orientation: SplitOrientation,
        ratio: SplitRatio,
        a: Box<LayoutNode<Id>>,
        b: Box<LayoutNode<Id>>,
    },
}

impl<Id: LeafId> LayoutNode<Id> {
    /// Convenience constructor for a leaf.
    #[must_use]
    pub fn leaf(pane: Id) -> Self {
        Self::Leaf { pane }
    }

    /// Convenience constructor for a balanced (`ratio = 0.5`) split.
    #[must_use]
    pub fn split(orientation: SplitOrientation, a: LayoutNode<Id>, b: LayoutNode<Id>) -> Self {
        Self::Split {
            orientation,
            ratio: SplitRatio::BALANCED,
            a: Box::new(a),
            b: Box::new(b),
        }
    }

    /// Collect every pane id reachable from this node, in left-to-
    /// right (then top-to-bottom) traversal order. Useful for
    /// rendering a status bar that wants "the active window's panes
    /// in display order".
    #[must_use]
    pub fn panes(&self) -> Vec<Id> {
        let mut out = Vec::new();
        self.collect(&mut out);
        out
    }

    fn collect(&self, out: &mut Vec<Id>) {
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
    fn is_leaf_of(&self, pane: Id) -> bool {
        matches!(self, Self::Leaf { pane: p } if *p == pane)
    }

    /// True when `pane` is one of this subtree's leaves.
    #[must_use]
    pub fn contains_pane(&self, pane: Id) -> bool {
        match self {
            Self::Leaf { pane: p } => *p == pane,
            Self::Split { a, b, .. } => a.contains_pane(pane) || b.contains_pane(pane),
        }
    }

    /// Arrange an ordered slice of panes into a named [`LayoutKind`]
    /// preset — the live-tree twin of [`crate::LayoutPlan::realize`]. The
    /// result always [`validate`](Self::validate)s and (by the shipped
    /// tiling property) `compute_rects` tiles its bounds exactly. `Even*`
    /// arrangements give every pane a `1/n` share (ratio `1/(n-k)` per
    /// split level), so an even-3 row is three equal thirds, not a
    /// balanced binary `0.5` split.
    ///
    /// Returns `None` for an empty slice and for [`LayoutKind::Custom`]
    /// (whose tree is its own source of truth — there is no canonical
    /// arrangement to build). A single pane yields a leaf for any kind.
    #[must_use]
    pub fn from_kind(kind: LayoutKind, panes: &[Id]) -> Option<Self> {
        match panes {
            [] => None,
            [only] => Some(Self::leaf(*only)),
            [main, rest @ ..] => match kind {
                // A horizontal ROW = panes side by side = Vertical splits.
                LayoutKind::EvenHorizontal => {
                    even_chain(SplitOrientation::Vertical, &leaves(panes))
                }
                // A vertical COLUMN = panes stacked = Horizontal splits.
                LayoutKind::EvenVertical => {
                    even_chain(SplitOrientation::Horizontal, &leaves(panes))
                }
                // One big pane on top; the rest share the bottom row.
                LayoutKind::MainHorizontal => {
                    let bottom = even_chain(SplitOrientation::Vertical, &leaves(rest))?;
                    Some(Self::Split {
                        orientation: SplitOrientation::Horizontal,
                        ratio: SplitRatio::BALANCED,
                        a: Box::new(Self::leaf(*main)),
                        b: Box::new(bottom),
                    })
                }
                // One big pane on the left; the rest stack on the right.
                LayoutKind::MainVertical => {
                    let right = even_chain(SplitOrientation::Horizontal, &leaves(rest))?;
                    Some(Self::Split {
                        orientation: SplitOrientation::Vertical,
                        ratio: SplitRatio::BALANCED,
                        a: Box::new(Self::leaf(*main)),
                        b: Box::new(right),
                    })
                }
                LayoutKind::Tiled => tiled(panes),
                // No canonical arrangement — the operator's manual tree wins.
                LayoutKind::Custom => None,
            },
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
        target: Id,
        new_pane: Id,
        direction: Direction,
        origin_ratio: f32,
    ) -> bool {
        match self {
            Self::Leaf { pane } if *pane == target => {
                let origin = Self::leaf(*pane);
                let fresh = Self::leaf(new_pane);
                // Refinement (non-finite → balanced, then clamp) now lives
                // in `SplitRatio::new`, so this site cannot get it wrong and
                // neither can any future one.
                let keep = SplitRatio::new(origin_ratio).get();
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
                    ratio: SplitRatio::new(ratio),
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
    pub fn remove_leaf(&mut self, target: Id) -> LeafRemoval {
        match self {
            Self::Leaf { pane } if *pane == target => LeafRemoval::WasRoot,
            Self::Leaf { .. } => LeafRemoval::NotFound,
            Self::Split { a, b, .. } => {
                // A direct child that *is* the target leaf collapses this
                // split into the other child.
                // ★ THE PLACEHOLDER IS THE LEAF WE ARE REMOVING, NOT A NULL.
                //
                // `*self = *survivor` needs an owned value to leave behind,
                // because `self` is borrowed. tear's version supplied a
                // `PaneId::NULL` sentinel — which then had to be policed by
                // `validate`, because a null leaf became a representable and
                // therefore reachable state.
                //
                // There is no need for one. `target` is the id of the leaf
                // being deleted and is `Copy`, so a leaf holding it is a
                // perfectly ordinary node — and it is dropped in the same
                // expression that creates it, along with the split. No
                // sentinel, no new illegal state, and no trait constant every
                // consumer would have to invent a value for.
                if a.is_leaf_of(target) {
                    *self = std::mem::replace(b.as_mut(), Self::leaf(target));
                    return LeafRemoval::Removed;
                }
                if b.is_leaf_of(target) {
                    *self = std::mem::replace(a.as_mut(), Self::leaf(target));
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
    pub fn resize_leaf(&mut self, target: Id, direction: Direction, delta_frac: f32) -> bool {
        let want = direction.orientation();
        // We enlarge `target` toward `direction`, so we want the divider
        // it shares with its neighbour ON THAT SIDE. The neighbour lives
        // on side `b` for Right/Below and side `a` for Left/Above — so we
        // need the deepest matching-orientation split where `target`
        // descends into the OPPOSITE (its own) side: `a` when growing
        // toward b, `b` when growing toward a. If no such split exists,
        // `target` is at the window edge that way — a no-op (false).
        let toward_b = matches!(direction, Direction::Right | Direction::Below);
        // Two-pass to satisfy the borrow checker: locate the path (shared
        // borrow), then mutate the ratio (exclusive borrow).
        let Some(path) = self.governing_split_path(target, want, toward_b) else {
            return false;
        };
        let Some(Self::Split { ratio, .. }) = self.split_at_path(&path) else {
            return false;
        };
        // Growing toward b raises the ratio (side `a` = target grows);
        // toward a lowers it (side `b` = target grows). A non-finite delta
        // leaves the ratio untouched — `SplitRatio::new` would coerce it to
        // BALANCED, which for a *resize* would silently re-centre a divider
        // the operator had placed, so that case is caught here instead.
        let sign = if toward_b { 1.0 } else { -1.0 };
        let next = ratio.get() + sign * delta_frac;
        if next.is_finite() {
            *ratio = SplitRatio::new(next);
        }
        true
    }

    /// Path to the deepest split of orientation `want` that contains
    /// `target` on the required side (`need_side_a` ⇒ `target` in side
    /// `a`, else side `b`). That is the divider `target` shares with its
    /// neighbour on the opposite side. `true` in a path step means "into
    /// side a". `None` when no such split exists (window edge that way).
    fn governing_split_path(
        &self,
        target: Id,
        want: SplitOrientation,
        need_side_a: bool,
    ) -> Option<Vec<bool>> {
        let mut best: Option<Vec<bool>> = None;
        let mut path: Vec<bool> = Vec::new();
        self.walk_governing(target, want, need_side_a, &mut path, &mut best);
        best
    }

    fn walk_governing(
        &self,
        target: Id,
        want: SplitOrientation,
        need_side_a: bool,
        path: &mut Vec<bool>,
        best: &mut Option<Vec<bool>>,
    ) {
        if let Self::Split {
            orientation, a, b, ..
        } = self
        {
            let in_a = a.contains_pane(target);
            let in_b = b.contains_pane(target);
            // Record only a split of the wanted orientation where target
            // is on the required side — that's the divider toward the
            // neighbour we grow into. Deeper (closer) overwrites shallower.
            if *orientation == want && ((in_a && need_side_a) || (in_b && !need_side_a)) {
                *best = Some(path.clone());
            }
            if in_a {
                path.push(true);
                a.walk_governing(target, want, need_side_a, path, best);
                path.pop();
            } else if in_b {
                path.push(false);
                b.walk_governing(target, want, need_side_a, path, best);
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
    pub fn compute_rects(&self, bounds: Rect) -> Vec<(Id, Rect)> {
        let mut out = Vec::with_capacity(self.pane_count());
        self.lay_out(bounds, &mut out);
        out
    }

    fn lay_out(&self, bounds: Rect, out: &mut Vec<(Id, Rect)>) {
        match self {
            Self::Leaf { pane } => out.push((*pane, bounds)),
            Self::Split {
                orientation,
                ratio,
                a,
                b,
            } => {
                let (ra, rb) = split_rect(bounds, *orientation, ratio.get());
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
    pub fn neighbor(&self, target: Id, direction: Direction, bounds: Rect) -> Option<Id> {
        let rects = self.compute_rects(bounds);
        let me = rects.iter().find(|(p, _)| *p == target).map(|(_, r)| *r)?;
        // Score each candidate as (gap, overlap): a candidate is better
        // when it is closer (smaller gap) or — at equal gap — shares more
        // edge (larger overlap). `best` holds the winning (gap, overlap).
        let mut best: Option<(Id, u32, u32)> = None;
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
    pub fn validate(&self) -> Result<(), LayoutError<Id>> {
        let mut seen = Vec::new();
        self.validate_into(&mut seen)
    }

    fn validate_into(&self, seen: &mut Vec<Id>) -> Result<(), LayoutError<Id>> {
        match self {
            Self::Leaf { pane } => {
                // (No null-leaf arm. tear's version had one because
                // `remove_leaf` planted a `PaneId::NULL` placeholder that
                // could survive a bug; this crate's collapse plants the
                // removed leaf instead, so the state has no producer left to
                // guard against — and a generic leaf id has no null to test.)
                if seen.contains(pane) {
                    return Err(LayoutError::DuplicatePane(*pane));
                }
                seen.push(*pane);
                Ok(())
            }
            Self::Split { ratio, a, b, .. } => {
                // Defence in depth only. `SplitRatio` has no constructor that
                // yields a value outside `[MIN_RATIO, 1 - MIN_RATIO]`, and
                // its `Deserialize` routes through that constructor, so this
                // branch is now UNREACHABLE from any real tree — kept so a
                // future change that widens the refinement is caught here
                // rather than in `split_extent`.
                let r = ratio.get();
                if !(r > 0.0 && r < 1.0) {
                    return Err(LayoutError::BadRatio(r));
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

/// Wrap each pane id in a leaf node.
fn leaves<Id: LeafId>(panes: &[Id]) -> Vec<LayoutNode<Id>> {
    panes.iter().map(|p| LayoutNode::<Id>::leaf(*p)).collect()
}

/// Fold a slice of subtrees into an *even* chain along `orientation`: the
/// first child gets `1/n` of the area and the remainder recurses, so —
/// because each level's `1/(n-k)` is taken of the previous remainder —
/// every leaf ends with exactly `1/n`. `None` for an empty slice.
fn even_chain<Id: LeafId>(
    orientation: SplitOrientation,
    nodes: &[LayoutNode<Id>],
) -> Option<LayoutNode<Id>> {
    match nodes {
        [] => None,
        [single] => Some(single.clone()),
        [first, rest @ ..] => {
            let n = nodes.len() as f32;
            let rest_tree = even_chain(orientation, rest)?;
            Some(LayoutNode::<Id>::Split {
                orientation,
                ratio: SplitRatio::new(1.0 / n),
                a: Box::new(first.clone()),
                b: Box::new(rest_tree),
            })
        }
    }
}

/// Arrange panes in an approximate square grid: `ceil(sqrt(n))` rows, each
/// an even row (side by side), the rows stacked evenly. `None` for empty.
fn tiled<Id: LeafId>(panes: &[Id]) -> Option<LayoutNode<Id>> {
    let n = panes.len();
    if n == 0 {
        return None;
    }
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let rows = (n as f64).sqrt().ceil() as usize;
    let per = n / rows;
    let extra = n % rows;
    let mut row_trees = Vec::with_capacity(rows);
    let mut i = 0;
    for r in 0..rows {
        // The first `extra` rows carry one more pane than the rest.
        let cnt = per + usize::from(r < extra);
        let row = even_chain(SplitOrientation::Vertical, &leaves(&panes[i..i + cnt]))?;
        row_trees.push(row);
        i += cnt;
    }
    even_chain(SplitOrientation::Horizontal, &row_trees)
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
    /// Custom: the [`LayoutNode`] tree on the the consumer's window is
    /// the source of truth. Operators reach this state after manual
    /// splits / resizes; tear-core serialises the tree as the canonical
    /// shape.
    Custom,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::direction::SplitOrientation;

    /// A leaf id that belongs to nobody.
    ///
    /// ★ THIS IS THE POINT OF THE EXTRACTION, NOT A TEST FIXTURE. These
    /// cases were written against tear's `PaneId` and are reproduced here
    /// verbatim against a type this crate invented — so passing them proves
    /// the algebra never depended on what a leaf *is*. Had any case needed a
    /// pane-specific fact, it would fail to compile here rather than quietly
    /// carrying tear's vocabulary into every future consumer.
    #[derive(Copy, Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
    struct Id(u64);

    #[test]
    fn leaf_has_one_pane() {
        let n = LayoutNode::<Id>::leaf(Id(7));
        assert_eq!(n.pane_count(), 1);
        assert_eq!(n.panes(), vec![Id(7)]);
    }

    #[test]
    fn split_aggregates_panes_left_then_right() {
        let n = LayoutNode::<Id>::split(
            SplitOrientation::Vertical,
            LayoutNode::<Id>::leaf(Id(1)),
            LayoutNode::<Id>::leaf(Id(2)),
        );
        assert_eq!(n.pane_count(), 2);
        assert_eq!(n.panes(), vec![Id(1), Id(2)]);
    }

    #[test]
    fn nested_split_traversal_is_predictable() {
        let n = LayoutNode::<Id>::split(
            SplitOrientation::Horizontal,
            LayoutNode::<Id>::leaf(Id(1)),
            LayoutNode::<Id>::split(
                SplitOrientation::Vertical,
                LayoutNode::<Id>::leaf(Id(2)),
                LayoutNode::<Id>::leaf(Id(3)),
            ),
        );
        assert_eq!(n.panes(), vec![Id(1), Id(2), Id(3)]);
        assert_eq!(n.pane_count(), 3);
    }

    // ── split_leaf ───────────────────────────────────────────────

    #[test]
    fn split_leaf_targets_the_matched_leaf_only() {
        // Tree: 1 | (2 / 3). Splitting pane 2 to the right must touch
        // ONLY pane 2's slot — panes 1 and 3 keep their positions. This
        // is the bug the old whole-window re-wrap had.
        let mut n = LayoutNode::<Id>::split(
            SplitOrientation::Vertical,
            LayoutNode::<Id>::leaf(Id(1)),
            LayoutNode::<Id>::split(
                SplitOrientation::Horizontal,
                LayoutNode::<Id>::leaf(Id(2)),
                LayoutNode::<Id>::leaf(Id(3)),
            ),
        );
        assert!(n.split_leaf(Id(2), Id(9), Direction::Right, 0.5));
        // Display order: 1, then (2-split-9), then 3.
        assert_eq!(n.panes(), vec![Id(1), Id(2), Id(9), Id(3)]);
        assert_eq!(n.pane_count(), 4);
        n.validate().unwrap();
    }

    #[test]
    fn split_leaf_orders_new_pane_by_direction() {
        let mut right = LayoutNode::<Id>::leaf(Id(1));
        right.split_leaf(Id(1), Id(2), Direction::Right, 0.5);
        assert_eq!(right.panes(), vec![Id(1), Id(2)]); // origin first

        let mut left = LayoutNode::<Id>::leaf(Id(1));
        left.split_leaf(Id(1), Id(2), Direction::Left, 0.5);
        assert_eq!(left.panes(), vec![Id(2), Id(1)]); // new first
    }

    #[test]
    fn split_leaf_unknown_target_is_noop() {
        let mut n = LayoutNode::<Id>::leaf(Id(1));
        assert!(!n.split_leaf(Id(99), Id(2), Direction::Right, 0.5));
        assert_eq!(n.panes(), vec![Id(1)]);
    }

    #[test]
    fn split_leaf_clamps_extreme_ratio() {
        let mut n = LayoutNode::<Id>::leaf(Id(1));
        n.split_leaf(Id(1), Id(2), Direction::Right, 0.0);
        // 0.0 origin-keep clamps to MIN_RATIO — still a valid split.
        n.validate().unwrap();
        if let LayoutNode::<Id>::Split { ratio, .. } = n {
            assert!(ratio.get() >= MIN_RATIO && ratio.get() <= 1.0 - MIN_RATIO);
        } else {
            panic!("expected a split");
        }
    }

    // ── remove_leaf ──────────────────────────────────────────────

    #[test]
    fn remove_leaf_collapses_parent_into_sibling() {
        // (1 | 2) → remove 1 → just 2 (no dangling NULL).
        let mut n = LayoutNode::<Id>::split(
            SplitOrientation::Vertical,
            LayoutNode::<Id>::leaf(Id(1)),
            LayoutNode::<Id>::leaf(Id(2)),
        );
        assert_eq!(n.remove_leaf(Id(1)), LeafRemoval::Removed);
        assert_eq!(n, LayoutNode::<Id>::leaf(Id(2)));
        n.validate().unwrap();
    }

    #[test]
    fn remove_leaf_collapses_deep_node() {
        // 1 | (2 / 3) → remove 3 → 1 | 2.
        let mut n = LayoutNode::<Id>::split(
            SplitOrientation::Vertical,
            LayoutNode::<Id>::leaf(Id(1)),
            LayoutNode::<Id>::split(
                SplitOrientation::Horizontal,
                LayoutNode::<Id>::leaf(Id(2)),
                LayoutNode::<Id>::leaf(Id(3)),
            ),
        );
        assert_eq!(n.remove_leaf(Id(3)), LeafRemoval::Removed);
        assert_eq!(n.panes(), vec![Id(1), Id(2)]);
        assert!(!n.contains_pane(Id(3)));
        // No leaf id::NULL leaf left behind.
        n.validate().unwrap();
    }

    #[test]
    fn remove_leaf_root_reports_was_root() {
        let mut n = LayoutNode::<Id>::leaf(Id(1));
        assert_eq!(n.remove_leaf(Id(1)), LeafRemoval::WasRoot);
        // Tree untouched — caller closes the window instead.
        assert_eq!(n, LayoutNode::<Id>::leaf(Id(1)));
    }

    #[test]
    fn remove_leaf_unknown_is_not_found() {
        let mut n = LayoutNode::<Id>::split(
            SplitOrientation::Vertical,
            LayoutNode::<Id>::leaf(Id(1)),
            LayoutNode::<Id>::leaf(Id(2)),
        );
        assert_eq!(n.remove_leaf(Id(99)), LeafRemoval::NotFound);
        assert_eq!(n.pane_count(), 2);
    }

    // ── compute_rects ────────────────────────────────────────────

    #[test]
    fn compute_rects_single_pane_fills_bounds() {
        let n = LayoutNode::<Id>::leaf(Id(1));
        let r = n.compute_rects(Rect::sized(80, 24));
        assert_eq!(r, vec![(Id(1), Rect::new(0, 0, 80, 24))]);
    }

    #[test]
    fn compute_rects_vertical_split_is_side_by_side_gapless() {
        // Vertical split divides width; a left, b right; no gap/overlap.
        let n = LayoutNode::<Id>::split(
            SplitOrientation::Vertical,
            LayoutNode::<Id>::leaf(Id(1)),
            LayoutNode::<Id>::leaf(Id(2)),
        );
        let r = n.compute_rects(Rect::sized(80, 24));
        let a = r.iter().find(|(p, _)| *p == Id(1)).unwrap().1;
        let b = r.iter().find(|(p, _)| *p == Id(2)).unwrap().1;
        assert_eq!(a, Rect::new(0, 0, 40, 24));
        assert_eq!(b, Rect::new(40, 0, 40, 24));
        assert_eq!(a.right(), u32::from(b.x)); // gapless
    }

    #[test]
    fn compute_rects_horizontal_split_stacks_rows() {
        let n = LayoutNode::<Id>::split(
            SplitOrientation::Horizontal,
            LayoutNode::<Id>::leaf(Id(1)),
            LayoutNode::<Id>::leaf(Id(2)),
        );
        let r = n.compute_rects(Rect::sized(80, 24));
        let a = r.iter().find(|(p, _)| *p == Id(1)).unwrap().1;
        let b = r.iter().find(|(p, _)| *p == Id(2)).unwrap().1;
        assert_eq!(a, Rect::new(0, 0, 80, 12));
        assert_eq!(b, Rect::new(0, 12, 80, 12));
        assert_eq!(a.bottom(), u32::from(b.y));
    }

    #[test]
    fn compute_rects_tiles_bounds_exactly() {
        // The union of all leaf rects must equal bounds.area() with no
        // double-counting — proves gap-free + overlap-free for a nested
        // tree at an odd size (forces rounding).
        let n = LayoutNode::<Id>::split(
            SplitOrientation::Vertical,
            LayoutNode::<Id>::leaf(Id(1)),
            LayoutNode::<Id>::split(
                SplitOrientation::Horizontal,
                LayoutNode::<Id>::leaf(Id(2)),
                LayoutNode::<Id>::leaf(Id(3)),
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
        let n = LayoutNode::<Id>::split(
            SplitOrientation::Vertical,
            LayoutNode::<Id>::leaf(Id(1)),
            LayoutNode::<Id>::leaf(Id(2)),
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
        let n = LayoutNode::<Id>::split(
            SplitOrientation::Vertical,
            LayoutNode::<Id>::leaf(Id(1)),
            LayoutNode::<Id>::split(
                SplitOrientation::Vertical,
                LayoutNode::<Id>::leaf(Id(2)),
                LayoutNode::<Id>::leaf(Id(3)),
            ),
        );
        let b = Rect::sized(90, 24);
        assert_eq!(n.neighbor(Id(2), Direction::Left, b), Some(Id(1)));
        assert_eq!(n.neighbor(Id(2), Direction::Right, b), Some(Id(3)));
        assert_eq!(n.neighbor(Id(1), Direction::Left, b), None); // window edge
        assert_eq!(n.neighbor(Id(3), Direction::Right, b), None);
    }

    #[test]
    fn neighbor_crosses_split_boundary_by_edge_overlap() {
        // Left column = pane 1 (full height). Right column split into
        // 2 (top) / 3 (bottom). From 1 going Right, the neighbour with
        // the most shared edge is whichever covers the focal band; both
        // 2 and 3 are valid candidates — assert it returns one of them.
        let n = LayoutNode::<Id>::split(
            SplitOrientation::Vertical,
            LayoutNode::<Id>::leaf(Id(1)),
            LayoutNode::<Id>::split(
                SplitOrientation::Horizontal,
                LayoutNode::<Id>::leaf(Id(2)),
                LayoutNode::<Id>::leaf(Id(3)),
            ),
        );
        let b = Rect::sized(80, 24);
        let right = n.neighbor(Id(1), Direction::Right, b);
        assert!(matches!(right, Some(Id(2)) | Some(Id(3))));
        // And from 2 going Left lands back on 1.
        assert_eq!(n.neighbor(Id(2), Direction::Left, b), Some(Id(1)));
    }

    #[test]
    fn neighbor_unknown_target_is_none() {
        let n = LayoutNode::<Id>::leaf(Id(1));
        assert_eq!(n.neighbor(Id(99), Direction::Left, Rect::sized(80, 24)), None);
    }

    #[test]
    fn neighbor_prefers_nearest_not_just_max_overlap() {
        // 1 | 2 | 3 — from 1, both 2 and 3 are to the Right with equal
        // (full-height) overlap. The NEAREST (2) must win regardless of
        // how the tree nests 2 and 3 (right-leaning vs left-leaning).
        let right_leaning = LayoutNode::<Id>::split(
            SplitOrientation::Vertical,
            LayoutNode::<Id>::leaf(Id(1)),
            LayoutNode::<Id>::split(
                SplitOrientation::Vertical,
                LayoutNode::<Id>::leaf(Id(2)),
                LayoutNode::<Id>::leaf(Id(3)),
            ),
        );
        let left_leaning = LayoutNode::<Id>::split(
            SplitOrientation::Vertical,
            LayoutNode::<Id>::split(
                SplitOrientation::Vertical,
                LayoutNode::<Id>::leaf(Id(1)),
                LayoutNode::<Id>::leaf(Id(2)),
            ),
            LayoutNode::<Id>::leaf(Id(3)),
        );
        let b = Rect::sized(90, 24);
        assert_eq!(right_leaning.neighbor(Id(1), Direction::Right, b), Some(Id(2)));
        assert_eq!(left_leaning.neighbor(Id(1), Direction::Right, b), Some(Id(2)));
    }

    // ── from_kind ────────────────────────────────────────────────

    #[test]
    fn from_kind_empty_and_custom_are_none() {
        assert_eq!(LayoutNode::<Id>::from_kind(LayoutKind::Tiled, &[]), None);
        assert_eq!(
            LayoutNode::<Id>::from_kind(LayoutKind::Custom, &[Id(1), Id(2)]),
            None
        );
    }

    #[test]
    fn from_kind_single_pane_is_a_leaf_for_every_kind() {
        for kind in [
            LayoutKind::EvenHorizontal,
            LayoutKind::EvenVertical,
            LayoutKind::MainHorizontal,
            LayoutKind::MainVertical,
            LayoutKind::Tiled,
        ] {
            assert_eq!(
                LayoutNode::<Id>::from_kind(kind, &[Id(1)]),
                Some(LayoutNode::<Id>::leaf(Id(1)))
            );
        }
    }

    #[test]
    fn from_kind_even_horizontal_gives_equal_thirds() {
        // The even-ratio refinement: 3 panes in a row are three EQUAL
        // widths, not a 50/25/25 balanced binary split.
        let panes = [Id(1), Id(2), Id(3)];
        let tree = LayoutNode::<Id>::from_kind(LayoutKind::EvenHorizontal, &panes).unwrap();
        tree.validate().unwrap();
        let rects = tree.compute_rects(Rect::sized(90, 24));
        let mut widths: Vec<u16> = rects.iter().map(|(_, r)| r.w).collect();
        widths.sort_unstable();
        // 90/3 = 30 each (exactly, since 90 is divisible by 3).
        assert_eq!(widths, vec![30, 30, 30]);
    }

    /// The first-brick proof: for EVERY non-Custom kind and pane-count
    /// 1..=7, `from_kind` produces a tree that validates AND whose
    /// `compute_rects` tiles its bounds exactly (the shipped tiling
    /// invariant) — gap-free, overlap-free, every pane present once.
    #[test]
    fn from_kind_every_preset_validates_and_tiles_exactly() {
        let kinds = [
            LayoutKind::EvenHorizontal,
            LayoutKind::EvenVertical,
            LayoutKind::MainHorizontal,
            LayoutKind::MainVertical,
            LayoutKind::Tiled,
        ];
        for kind in kinds {
            for n in 1..=7usize {
                let panes: Vec<Id> = (1..=n as u64).map(Id).collect();
                let tree = LayoutNode::<Id>::from_kind(kind, &panes)
                    .unwrap_or_else(|| panic!("{kind:?} n={n} produced None"));
                // Structurally sound + every pane present exactly once.
                tree.validate()
                    .unwrap_or_else(|e| panic!("{kind:?} n={n} invalid: {e:?}"));
                assert_eq!(tree.pane_count(), n, "{kind:?} n={n} pane count");
                assert_eq!(tree.panes().len(), n);
                // Tiles bounds exactly at a rounding-forcing size.
                for &(w, h) in &[(80u16, 24u16), (81, 25), (97, 31)] {
                    let bounds = Rect::sized(w, h);
                    let rects = tree.compute_rects(bounds);
                    let total: u32 = rects.iter().map(|(_, r)| r.area()).sum();
                    assert_eq!(total, bounds.area(), "{kind:?} n={n} at {w}x{h}");
                }
            }
        }
    }

    #[test]
    fn from_kind_main_vertical_keeps_main_on_the_left() {
        let panes = [Id(1), Id(2), Id(3)];
        let tree = LayoutNode::<Id>::from_kind(LayoutKind::MainVertical, &panes).unwrap();
        // Outer split is vertical (left|right), side a is the main leaf.
        match &tree {
            LayoutNode::<Id>::Split { orientation, a, .. } => {
                assert_eq!(*orientation, SplitOrientation::Vertical);
                assert_eq!(a.as_ref(), &LayoutNode::<Id>::leaf(Id(1)));
            }
            LayoutNode::<Id>::Leaf { .. } => panic!("expected a split"),
        }
        // The main pane is the leftmost on screen.
        let rects = tree.compute_rects(Rect::sized(80, 24));
        let main = rects.iter().find(|(p, _)| *p == Id(1)).unwrap().1;
        assert_eq!(main.x, 0);
    }

    /// A pseudo-property test: across a spread of tree shapes and bounds
    /// (including odd sizes that force rounding), `compute_rects` must
    /// tile `bounds` exactly — total area equals `bounds.area()` and no
    /// interior cell is owned by two panes. This is the load-bearing
    /// invariant both apps depend on (no seams, no double-draws).
    #[test]
    fn compute_rects_tiles_exactly_across_shapes_and_sizes() {
        let shapes = [
            LayoutNode::<Id>::leaf(Id(1)),
            LayoutNode::<Id>::split(
                SplitOrientation::Vertical,
                LayoutNode::<Id>::leaf(Id(1)),
                LayoutNode::<Id>::leaf(Id(2)),
            ),
            LayoutNode::<Id>::split(
                SplitOrientation::Horizontal,
                LayoutNode::<Id>::split(
                    SplitOrientation::Vertical,
                    LayoutNode::<Id>::leaf(Id(1)),
                    LayoutNode::<Id>::leaf(Id(2)),
                ),
                LayoutNode::<Id>::split(
                    SplitOrientation::Vertical,
                    LayoutNode::<Id>::leaf(Id(3)),
                    LayoutNode::<Id>::split(
                        SplitOrientation::Horizontal,
                        LayoutNode::<Id>::leaf(Id(4)),
                        LayoutNode::<Id>::leaf(Id(5)),
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
        let mut n = LayoutNode::<Id>::split(
            SplitOrientation::Vertical,
            LayoutNode::<Id>::leaf(Id(1)),
            LayoutNode::<Id>::leaf(Id(2)),
        );
        let before = n.compute_rects(Rect::sized(80, 24));
        let w1_before = before.iter().find(|(p, _)| *p == Id(1)).unwrap().1.w;
        assert!(n.resize_leaf(Id(1), Direction::Right, 0.2));
        let after = n.compute_rects(Rect::sized(80, 24));
        let w1_after = after.iter().find(|(p, _)| *p == Id(1)).unwrap().1.w;
        assert!(w1_after > w1_before, "{w1_after} !> {w1_before}");
    }

    #[test]
    fn resize_leaf_grows_pane_on_side_b_too() {
        // 1 | 2; "grow 2 to the Left" enlarges pane 2 (side b).
        let mut n = LayoutNode::<Id>::split(
            SplitOrientation::Vertical,
            LayoutNode::<Id>::leaf(Id(1)),
            LayoutNode::<Id>::leaf(Id(2)),
        );
        let before = n.compute_rects(Rect::sized(80, 24));
        let w2_before = before.iter().find(|(p, _)| *p == Id(2)).unwrap().1.w;
        assert!(n.resize_leaf(Id(2), Direction::Left, 0.2));
        let after = n.compute_rects(Rect::sized(80, 24));
        let w2_after = after.iter().find(|(p, _)| *p == Id(2)).unwrap().1.w;
        assert!(w2_after > w2_before, "{w2_after} !> {w2_before}");
    }

    #[test]
    fn resize_leaf_grows_toward_outer_neighbour_across_a_deeper_split() {
        // 1 | (2 | 3): pane 2 is the LEFT child of the inner split, but
        // its on-screen left neighbour is pane 1 across the OUTER split.
        // "grow 2 Left" must enlarge pane 2 (regression: the old sign-only
        // logic slid the inner 2|3 divider and SHRANK pane 2).
        let mut n = LayoutNode::<Id>::split(
            SplitOrientation::Vertical,
            LayoutNode::<Id>::leaf(Id(1)),
            LayoutNode::<Id>::split(
                SplitOrientation::Vertical,
                LayoutNode::<Id>::leaf(Id(2)),
                LayoutNode::<Id>::leaf(Id(3)),
            ),
        );
        let b = Rect::sized(90, 24);
        let w2_before = n.compute_rects(b).iter().find(|(p, _)| *p == Id(2)).unwrap().1.w;
        assert!(n.resize_leaf(Id(2), Direction::Left, 0.2));
        let w2_after = n.compute_rects(b).iter().find(|(p, _)| *p == Id(2)).unwrap().1.w;
        assert!(w2_after > w2_before, "grow-Left should enlarge pane 2: {w2_before} -> {w2_after}");
    }

    #[test]
    fn resize_leaf_side_b_of_deeper_split_grows_right_toward_outer_neighbour() {
        // (1 | 2) | 3: pane 2 is the RIGHT child of the inner split; its
        // on-screen right neighbour is pane 3 across the OUTER split.
        // "grow 2 Right" must enlarge pane 2 (the symmetric regression).
        let mut n = LayoutNode::<Id>::split(
            SplitOrientation::Vertical,
            LayoutNode::<Id>::split(
                SplitOrientation::Vertical,
                LayoutNode::<Id>::leaf(Id(1)),
                LayoutNode::<Id>::leaf(Id(2)),
            ),
            LayoutNode::<Id>::leaf(Id(3)),
        );
        let b = Rect::sized(90, 24);
        let w2_before = n.compute_rects(b).iter().find(|(p, _)| *p == Id(2)).unwrap().1.w;
        assert!(n.resize_leaf(Id(2), Direction::Right, 0.2));
        let w2_after = n.compute_rects(b).iter().find(|(p, _)| *p == Id(2)).unwrap().1.w;
        assert!(w2_after > w2_before, "grow-Right should enlarge pane 2: {w2_before} -> {w2_after}");
    }

    #[test]
    fn resize_leaf_no_neighbour_that_way_is_noop() {
        // 1 | 2: pane 1 is leftmost — it has no left neighbour, so
        // "grow 1 Left" finds no governing divider and is a no-op.
        let mut n = LayoutNode::<Id>::split(
            SplitOrientation::Vertical,
            LayoutNode::<Id>::leaf(Id(1)),
            LayoutNode::<Id>::leaf(Id(2)),
        );
        assert!(!n.resize_leaf(Id(1), Direction::Left, 0.2));
    }

    #[test]
    fn resize_leaf_grows_focused_pane_in_all_four_directions() {
        // Exhaustive sign coverage. For each direction, build a layout
        // where the focused pane HAS a neighbour that way, and assert the
        // focused pane grows. This pins every (side × direction) sign
        // combination — the bug the original 3 tests missed.
        let cases: &[(SplitOrientation, Direction)] = &[
            (SplitOrientation::Vertical, Direction::Right),
            (SplitOrientation::Vertical, Direction::Left),
            (SplitOrientation::Horizontal, Direction::Below),
            (SplitOrientation::Horizontal, Direction::Above),
        ];
        for &(orient, dir) in cases {
            // Two-pane split on the matching axis; focus the pane that has
            // a neighbour in `dir` (the `a`-side pane for Right/Below, the
            // `b`-side pane for Left/Above).
            let (focus, other) = match dir {
                Direction::Right | Direction::Below => (Id(1), Id(2)),
                Direction::Left | Direction::Above => (Id(2), Id(1)),
            };
            let mut n = LayoutNode::<Id>::split(
                orient,
                LayoutNode::<Id>::leaf(Id(1)),
                LayoutNode::<Id>::leaf(Id(2)),
            );
            let b = Rect::sized(80, 24);
            let axis = |r: Rect| if orient == SplitOrientation::Vertical { r.w } else { r.h };
            let before = axis(n.compute_rects(b).iter().find(|(p, _)| *p == focus).unwrap().1);
            assert!(n.resize_leaf(focus, dir, 0.2), "{dir:?} should find a divider");
            let after = axis(n.compute_rects(b).iter().find(|(p, _)| *p == focus).unwrap().1);
            assert!(after > before, "focus pane should grow {dir:?}: {before} -> {after}");
            let _ = other;
        }
    }

    #[test]
    fn split_leaf_nan_ratio_coerces_to_valid_split() {
        // A NaN ratio survives f32::clamp; split_leaf must coerce it so the
        // resulting tree still validates (its doc promises a clamped ratio).
        let mut n = LayoutNode::<Id>::leaf(Id(1));
        assert!(n.split_leaf(Id(1), Id(2), Direction::Right, f32::NAN));
        n.validate().unwrap();
    }

    #[test]
    fn resize_leaf_nan_delta_leaves_tree_valid() {
        let mut n = LayoutNode::<Id>::split(
            SplitOrientation::Vertical,
            LayoutNode::<Id>::leaf(Id(1)),
            LayoutNode::<Id>::leaf(Id(2)),
        );
        // A NaN delta must not poison the ratio.
        n.resize_leaf(Id(1), Direction::Right, f32::NAN);
        n.validate().unwrap();
    }

    #[test]
    fn resize_leaf_ignores_wrong_axis() {
        // 1 | 2 vertical; resizing Up/Down finds no horizontal split.
        let mut n = LayoutNode::<Id>::split(
            SplitOrientation::Vertical,
            LayoutNode::<Id>::leaf(Id(1)),
            LayoutNode::<Id>::leaf(Id(2)),
        );
        assert!(!n.resize_leaf(Id(1), Direction::Below, 0.2));
    }

    #[test]
    fn resize_leaf_picks_deepest_governing_split() {
        // (1 | 2) stacked-over 3 — both a vertical and an enclosing
        // horizontal split exist. Resizing 1 to the Right must adjust
        // the inner VERTICAL split (1|2), not the outer horizontal one.
        let mut n = LayoutNode::<Id>::split(
            SplitOrientation::Horizontal,
            LayoutNode::<Id>::split(
                SplitOrientation::Vertical,
                LayoutNode::<Id>::leaf(Id(1)),
                LayoutNode::<Id>::leaf(Id(2)),
            ),
            LayoutNode::<Id>::leaf(Id(3)),
        );
        let bounds = Rect::sized(80, 24);
        let w1_before = n.compute_rects(bounds).iter().find(|(p, _)| *p == Id(1)).unwrap().1.w;
        let h3_before = n.compute_rects(bounds).iter().find(|(p, _)| *p == Id(3)).unwrap().1.h;
        assert!(n.resize_leaf(Id(1), Direction::Right, 0.2));
        let after = n.compute_rects(bounds);
        let w1_after = after.iter().find(|(p, _)| *p == Id(1)).unwrap().1.w;
        let h3_after = after.iter().find(|(p, _)| *p == Id(3)).unwrap().1.h;
        assert!(w1_after > w1_before); // inner split moved
        assert_eq!(h3_after, h3_before); // outer split untouched
    }

    // ── validate ─────────────────────────────────────────────────

    /// The test that REPLACED `validate_rejects_null_leaf`, and proves more.
    ///
    /// tear's version planted a `PaneId::NULL` placeholder during a collapse
    /// and had `validate` reject it if one ever escaped — a guard on a state
    /// the code itself could produce. This crate's collapse plants the leaf
    /// being removed, so the escape has no producer; what is worth asserting
    /// is therefore not "a null is rejected" but "nothing foreign is left
    /// behind at all". That is the stronger claim, and it is the one that
    /// would fail if the placeholder ever leaked.
    #[test]
    fn collapse_leaves_nothing_foreign_behind() {
        let mut n = LayoutNode::<Id>::split(
            SplitOrientation::Vertical,
            LayoutNode::leaf(Id(1)),
            LayoutNode::leaf(Id(2)),
        );
        assert_eq!(n.remove_leaf(Id(1)), LeafRemoval::Removed);
        assert_eq!(n.panes(), vec![Id(2)]);
        // The removed id must not survive as a placeholder — which is
        // exactly what `mem::replace(.., Self::leaf(target))` would leave if
        // the assignment order were ever inverted.
        assert!(!n.contains_pane(Id(1)));
        assert_eq!(n.validate(), Ok(()));
    }

    #[test]
    fn validate_rejects_duplicate_pane() {
        let n = LayoutNode::<Id>::split(
            SplitOrientation::Vertical,
            LayoutNode::<Id>::leaf(Id(5)),
            LayoutNode::<Id>::leaf(Id(5)),
        );
        assert_eq!(n.validate(), Err(LayoutError::DuplicatePane(Id(5))));
    }

    /// This test used to construct `ratio: 0.0` and assert `validate()`
    /// returned `BadRatio`. **That tree can no longer be written** — the
    /// field is a `SplitRatio` whose only constructor refines its input, so
    /// the degenerate value has no representation and the runtime check it
    /// used to exercise is unreachable.
    ///
    /// The tier moved from *validated* to *unrepresentable*, so the test
    /// moves with it: it now proves the refinement instead of the rejection.
    #[test]
    fn a_degenerate_ratio_has_no_representation() {
        for bad in [0.0, 1.0, -3.0, 42.0] {
            let r = SplitRatio::new(bad);
            assert!(
                r.get() >= MIN_RATIO && r.get() <= 1.0 - MIN_RATIO,
                "{bad} must refine into range, got {}",
                r.get()
            );
        }
        // And a tree built from them still validates, because there is no
        // way to put the bad value in.
        let n = LayoutNode::<Id>::Split {
            orientation: SplitOrientation::Vertical,
            ratio: SplitRatio::new(0.0),
            a: Box::new(LayoutNode::<Id>::leaf(Id(1))),
            b: Box::new(LayoutNode::<Id>::leaf(Id(2))),
        };
        n.validate().expect("a refined ratio always validates");
    }

    /// The silent one. `NaN` used to pass straight through `f32::clamp`
    /// (which returns NaN for NaN input rather than panicking), and
    /// `NaN as u16` saturates to `0` — so a NaN ratio did not error, it
    /// silently gave one side ZERO cells and the pane vanished.
    #[test]
    fn a_nan_ratio_cannot_reach_the_geometry() {
        assert_eq!(SplitRatio::new(f32::NAN).get(), SplitRatio::BALANCED.get());
        assert_eq!(SplitRatio::new(f32::INFINITY).get(), SplitRatio::BALANCED.get());
        assert_eq!(
            SplitRatio::new(f32::NEG_INFINITY).get(),
            SplitRatio::BALANCED.get()
        );

        // The end-to-end consequence: both sides get real cells.
        let n = LayoutNode::<Id>::Split {
            orientation: SplitOrientation::Vertical,
            ratio: SplitRatio::new(f32::NAN),
            a: Box::new(LayoutNode::<Id>::leaf(Id(1))),
            b: Box::new(LayoutNode::<Id>::leaf(Id(2))),
        };
        let rects = n.compute_rects(Rect::sized(80, 24));
        assert_eq!(rects.len(), 2);
        for (pane, r) in rects {
            assert!(r.w > 0 && r.h > 0, "pane {pane:?} vanished: {r:?}");
        }
    }

    /// A hostile or merely stale peer cannot put a bad ratio into a live
    /// tree: `Deserialize` routes through the same refinement.
    #[test]
    fn deserialisation_refines_a_hostile_ratio() {
        let zero: SplitRatio = serde_json::from_str("0.0").expect("deserialises");
        assert!(zero.get() >= MIN_RATIO, "wire value must be refined");

        let huge: SplitRatio = serde_json::from_str("42.0").expect("deserialises");
        assert!(huge.get() <= 1.0 - MIN_RATIO, "wire value must be refined");

        let neg: SplitRatio = serde_json::from_str("-3.0").expect("deserialises");
        assert!(neg.get() >= MIN_RATIO, "wire value must be refined");
    }

    /// ★ The compatibility claim, tested rather than asserted.
    ///
    /// `SplitRatio` replaced a bare `f32` on a type that crosses the CBOR
    /// wire between daemon and client, and is persisted to `praca.json`. If
    /// the encoding changed, every already-running daemon would fail to
    /// talk to a new client — a flag day. `#[serde(transparent)]` is what
    /// prevents that, and this pins it.
    #[test]
    fn split_ratio_is_wire_identical_to_the_bare_f32_it_replaced() {
        let as_ratio = serde_json::to_string(&SplitRatio::new(0.25)).unwrap();
        let as_f32 = serde_json::to_string(&0.25_f32).unwrap();
        assert_eq!(
            as_ratio, as_f32,
            "SplitRatio must serialise exactly like the f32 it replaced, or \
             a running daemon cannot talk to a new client"
        );

        // And a whole tree round-trips through the wire form unchanged.
        let tree = LayoutNode::<Id>::Split {
            orientation: SplitOrientation::Vertical,
            ratio: SplitRatio::new(0.25),
            a: Box::new(LayoutNode::<Id>::leaf(Id(1))),
            b: Box::new(LayoutNode::<Id>::leaf(Id(2))),
        };
        let json = serde_json::to_string(&tree).unwrap();
        let back: LayoutNode<Id> = serde_json::from_str(&json).unwrap();
        assert_eq!(back, tree);
    }

    #[test]
    fn validate_accepts_well_formed_tree() {
        let n = LayoutNode::<Id>::split(
            SplitOrientation::Vertical,
            LayoutNode::<Id>::leaf(Id(1)),
            LayoutNode::<Id>::split(
                SplitOrientation::Horizontal,
                LayoutNode::<Id>::leaf(Id(2)),
                LayoutNode::<Id>::leaf(Id(3)),
            ),
        );
        n.validate().unwrap();
    }

    // ── round-trip: split then remove returns to the original ────

    #[test]
    fn split_then_remove_is_identity() {
        let original = LayoutNode::<Id>::leaf(Id(1));
        let mut n = original.clone();
        n.split_leaf(Id(1), Id(2), Direction::Right, 0.5);
        assert_eq!(n.pane_count(), 2);
        assert_eq!(n.remove_leaf(Id(2)), LeafRemoval::Removed);
        assert_eq!(n, original);
    }
}
