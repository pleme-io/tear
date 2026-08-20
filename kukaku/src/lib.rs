//! kukaku (区画) — the split-tree layout algebra, owned by neither consumer.
//!
//! A *kukaku* is a plot of land: an area divided and subdivided, each parcel
//! addressable, the divisions themselves the thing you reason about. That is
//! what a tiling layout is, and it is deliberately not a word about windows or
//! panes — the whole point of this crate is that it knows about neither.
//!
//! ── ★ WHY IT IS ITS OWN CRATE ─────────────────────────────────────────────
//! This algebra was written inside `tear-types`, where a leaf was a terminal
//! pane. Its own module header said the plan was for mado to adopt it too:
//! *"mado's existing pane.rs/tab.rs uses a flat Vec<PaneRect>; the eventual M5
//! rebase swaps that for LayoutNode so a single algorithm computes pixel rects
//! for both apps."* A third consumer then arrived — `omoya`, the compositor,
//! which needs to tile windows on a screen — and three consumers of a
//! primitive living inside one of them is the shape the fleet extracts on.
//!
//! Reading the types rather than the prose settles what kind of sharing this
//! is. Every operation here uses exactly two facts about a leaf's identity:
//! it copies, and two can be compared. Nothing asks what a pane *is*. So this
//! is not two similar problems that could share code — it is one problem that
//! was wearing a multiplexer's vocabulary, and the vocabulary is all that had
//! to go.
//!
//! ── ★ WHAT IT DELIBERATELY DOES NOT KNOW ─────────────────────────────────
//! No pixels, no cells, no windows, no terminals, no I/O. [`Rect`] is unitless
//! `u16`, which is why the same `compute_rects` serves a 1024×768 framebuffer
//! and an 80×24 grid without either consumer converting anything. Its only
//! dependency is `serde`.
//!
//! ── ★ THE INVARIANT WORTH KNOWING BEFORE YOU TOUCH IT ────────────────────
//! [`SplitRatio`] is a refined newtype, not an `f32`, and the reason is
//! measured rather than stylistic: `f32::clamp` returns `NaN` for a `NaN`
//! input (it only panics when the *bounds* are NaN), and `NaN as u16`
//! saturates to `0`. A NaN ratio therefore did not panic and did not error —
//! one side silently got zero extent and the parcel vanished. Every route in,
//! including deserialisation, goes through [`SplitRatio::new`].

pub mod direction;
pub mod geometry;
pub mod layout;
pub mod leaf;

pub use direction::{Direction, SplitOrientation};
pub use geometry::Rect;
pub use layout::{LayoutError, LayoutKind, LayoutNode, LeafRemoval, MIN_RATIO, SplitRatio};
pub use leaf::{Leaf, LeafId};
