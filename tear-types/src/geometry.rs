//! Rectangles — re-exported from `kukaku`, where the layout algebra lives.
//!
//! `Rect` moved with `LayoutNode` because `compute_rects` returns them and a
//! second `Rect` type on this side would be exactly the "no second Rect type"
//! duplication the fleet forbids. It is deliberately unitless `u16`, which is
//! why the same algebra serves an 80×24 grid and a 1024×768 framebuffer with
//! neither consumer converting anything.

pub use kukaku::geometry::Rect;
