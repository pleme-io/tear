//! What a leaf has to BE for the split algebra to work on it.

use serde::{Deserialize, Serialize};

/// The identity of a leaf in a [`crate::LayoutNode`] tree.
///
/// ★ A TRAIT, NOT A CONCRETE ID, AND THAT IS THE WHOLE EXTRACTION.
///
/// This algebra came out of tear, where a leaf was a terminal *pane* and its
/// identity was `PaneId`. Every operation in `layout.rs` — split, remove,
/// resize, compute rects, find the neighbour in a direction — turned out to
/// use exactly two facts about that id: you can copy it, and you can tell two
/// apart. Nothing anywhere asked what a pane *was*.
///
/// So the id became a parameter and the crate stopped belonging to a
/// multiplexer. A compositor tiling windows and a multiplexer tiling panes are
/// not two similar problems that ought to share code; read through the types
/// they are the SAME problem, and the only thing that differed was the noun.
///
/// The blanket impl means a consumer writes no glue: any `Copy + Eq + Debug`
/// newtype is already a leaf id.
pub trait LeafId: Copy + Eq + core::fmt::Debug {}

impl<T: Copy + Eq + core::fmt::Debug> LeafId for T {}

/// A ready-made leaf id for a consumer that has no id type of its own.
///
/// Deliberately NOT used by this crate's own tests — those define their own,
/// so the tests prove the algebra works for a *foreign* id rather than for
/// this one.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Leaf(pub u64);
