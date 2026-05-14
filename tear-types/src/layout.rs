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

use crate::{direction::SplitOrientation, id::PaneId};

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
}
