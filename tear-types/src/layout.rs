//! Binary-tree layout for a window's panes.
//!
//! ── ★ THIS MODULE IS NOW A RE-EXPORT. THE ALGEBRA LIVES IN `kukaku`. ─────
//!
//! Every type here used to be defined in this file, against `PaneId`. It was
//! extracted when `omoya` — the compositor — became the third consumer, after
//! this module's own header had already named the second: *"mado's existing
//! pane.rs/tab.rs uses a flat Vec<PaneRect>; the eventual M5 rebase swaps that
//! for LayoutNode so a single algorithm computes pixel rects for both apps."*
//!
//! Reading the types settled what kind of sharing it is. The whole algebra
//! used exactly two facts about a pane's identity — it copies, and two can be
//! compared — and nothing asked what a pane *was*. That is one problem wearing
//! a multiplexer's vocabulary, not two problems that happen to rhyme, so the
//! id became a type parameter and the crate stopped belonging to tear.
//!
//! ── ★ NOTHING IN TEAR CHANGES ────────────────────────────────────────────
//! [`LayoutNode`] is a type ALIAS for `kukaku::LayoutNode<PaneId>`, so every
//! existing `LayoutNode::leaf(..)`, `LayoutNode::Split { .. }` and every
//! signature naming it compiles untouched — and the serde representation is
//! the same tagged enum over the same `PaneId`, so the CBOR wire is
//! byte-identical. This is a re-homing, not a format change.

pub use kukaku::layout::{LayoutError, LayoutKind, LeafRemoval, MIN_RATIO, SplitRatio};

use serde::{Deserialize, Serialize};

use crate::id::PaneId;

/// One node in a window's layout tree — `kukaku`'s tree with tear's pane id.
pub type LayoutNode = kukaku::LayoutNode<PaneId>;

/// Size specification for a pane within a layout.
///
/// ★ STAYED BEHIND WHEN THE ALGEBRA LEFT. `Size::Cells` counts TERMINAL
/// CELLS, which is a fact about terminals and not about splitting an area —
/// `kukaku` deliberately knows no unit at all, which is what lets the same
/// `compute_rects` serve an 80x24 grid and a 1024x768 framebuffer. Moving
/// this with the rest would have carried a terminal into a crate whose whole
/// point is not being one.
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
