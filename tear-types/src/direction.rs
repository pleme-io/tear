//! Directions and split orientation — re-exported from `kukaku`.
//!
//! These are the vocabulary the split algebra is written in (`resize_leaf`
//! takes a `Direction`, `neighbor` walks one), so they moved with it rather
//! than being duplicated on both sides of the seam.

pub use kukaku::direction::{Direction, SplitOrientation};
