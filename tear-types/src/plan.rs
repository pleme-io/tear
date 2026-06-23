//! The latent layout *plan* — what a session definition stores, before
//! it is instantiated into live panes.
//!
//! A [`crate::SessionDefinition`] (in praça) needs to describe a layout
//! WITHOUT naming any live [`PaneId`]: a stored definition that
//! referenced a runtime pane id would be nonsense the moment the daemon
//! restarts and mints fresh ids. So the plan is layout-as-data over
//! [`PaneSlot`]s — stable, definition-local keys — and the
//! [`LayoutPlan::realize`] morphism turns a plan into a live
//! [`LayoutNode`] by minting one [`PaneId`] per slot. After `realize`,
//! the whole shipped layout algebra (`compute_rects`, `neighbor`,
//! `split_leaf`, …) applies unchanged — the plan reuses, never
//! duplicates, the live tree.
//!
//! This is the typed fix for the pressure-test's illegal state #3: a
//! definition that carried *no* layout/panes/commands, so there was
//! nothing to instantiate. Here the layout and the per-pane spawn specs
//! are first-class, and a plan that references a live id is
//! **unrepresentable** — [`LayoutPlan`]'s leaves hold [`PaneSlot`], not
//! [`PaneId`].

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::{
    direction::SplitOrientation,
    id::PaneId,
    layout::LayoutNode,
    pane::InputPolicy,
};

/// A stable, definition-local pane slot key. Assigned at authoring time
/// (a small index), it identifies a pane *within a definition* — never a
/// live pane. The newtype means a slot can never be passed where a live
/// [`PaneId`] is expected (and vice-versa): the two id spaces don't mix.
#[derive(Copy, Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct PaneSlot(pub u32);

/// What to spawn in one slot when a definition is instantiated. This is
/// the per-pane data a stored definition must carry so a session can be
/// re-created (or re-instantiated after a restart) faithfully. Every
/// field mirrors the corresponding [`crate::TearPane`] field so the
/// `instantiate` interpreter maps `SpawnSpec` → `TearPane` directly.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SpawnSpec {
    /// The slot this spec fills — the join key to the [`LayoutPlan`].
    pub slot: PaneSlot,
    /// Shell/command to run (e.g. `"/run/current-system/sw/bin/zsh"`).
    pub shell: String,
    /// Arguments after the shell command.
    #[serde(default)]
    pub args: Vec<String>,
    /// Working directory at spawn; `None` inherits from the session.
    #[serde(default)]
    pub cwd: Option<String>,
    /// Per-pane environment overrides — same `Vec<(String, String)>`
    /// shape as [`crate::TearPane::env`].
    #[serde(default)]
    pub env: Vec<(String, String)>,
    /// Initial title.
    #[serde(default)]
    pub title: String,
    /// Input acceptance policy for the spawned pane.
    #[serde(default)]
    pub input_policy: InputPolicy,
}

impl SpawnSpec {
    /// A bare shell in a slot — the common single-pane case.
    #[must_use]
    pub fn shell(slot: PaneSlot, shell: impl Into<String>) -> Self {
        Self {
            slot,
            shell: shell.into(),
            args: Vec::new(),
            cwd: None,
            env: Vec::new(),
            title: String::new(),
            input_policy: InputPolicy::default(),
        }
    }
}

/// The latent layout: structurally the binary-tree twin of
/// [`LayoutNode`], but its leaves hold [`PaneSlot`] (a definition-local
/// key) instead of a live [`PaneId`]. A stored definition therefore
/// references nothing runtime — the design's deliberate choice over
/// generifying the shipped `LayoutNode<L>` (which would churn its whole
/// proptest suite + every call site). [`LayoutPlan::realize`] bridges the
/// two id spaces.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum LayoutPlan {
    /// One slot filling its box.
    Leaf { slot: PaneSlot },
    /// A split between side `a` (top/left) and side `b` (bottom/right);
    /// `ratio` is side `a`'s fraction, same convention as [`LayoutNode`].
    Split {
        orientation: SplitOrientation,
        ratio: f32,
        a: Box<LayoutPlan>,
        b: Box<LayoutPlan>,
    },
}

/// The latent mirror of a [`crate::TearWindow`]: a named window with a
/// layout plan and the slot that should be focused after instantiation.
/// Stored inside a session definition; carries no runtime ids — a
/// definition can hold several of these (multi-window sessions).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct WindowPlan {
    /// Window name (`work`, `logs`, …).
    pub name: String,
    /// The pane layout, over slots.
    pub layout: LayoutPlan,
    /// The slot that becomes `active_pane` after instantiation.
    pub active_slot: PaneSlot,
}

impl WindowPlan {
    /// A single-pane window over one slot, that slot active.
    #[must_use]
    pub fn single(name: impl Into<String>, slot: PaneSlot) -> Self {
        Self {
            name: name.into(),
            layout: LayoutPlan::leaf(slot),
            active_slot: slot,
        }
    }
}

/// Why a [`LayoutPlan`] failed [`LayoutPlan::validate`]. Mirrors the
/// shipped [`crate::LayoutError`] discipline for the plan's id space
/// (slots, not panes). A plan that fails to validate never reaches
/// `realize`, so a malformed definition can't produce a broken live tree.
// No `Eq` — `BadRatio(f32)` carries a float.
#[derive(Clone, Debug, PartialEq)]
pub enum PlanError {
    /// The same [`PaneSlot`] appears in two leaves — a slot fills exactly
    /// one box, so aliasing would map two panes onto one position.
    DuplicateSlot(PaneSlot),
    /// A split's ratio is not in the open interval `(0.0, 1.0)`.
    BadRatio(f32),
}

impl LayoutPlan {
    /// A single-slot plan.
    #[must_use]
    pub fn leaf(slot: PaneSlot) -> Self {
        Self::Leaf { slot }
    }

    /// A balanced (`ratio = 0.5`) split.
    #[must_use]
    pub fn split(orientation: SplitOrientation, a: LayoutPlan, b: LayoutPlan) -> Self {
        Self::Split {
            orientation,
            ratio: 0.5,
            a: Box::new(a),
            b: Box::new(b),
        }
    }

    /// Every slot in the plan, left-to-right (then top-to-bottom).
    #[must_use]
    pub fn slots(&self) -> Vec<PaneSlot> {
        let mut out = Vec::new();
        self.collect(&mut out);
        out
    }

    fn collect(&self, out: &mut Vec<PaneSlot>) {
        match self {
            Self::Leaf { slot } => out.push(*slot),
            Self::Split { a, b, .. } => {
                a.collect(out);
                b.collect(out);
            }
        }
    }

    /// Number of slots (= number of panes the plan will spawn).
    #[must_use]
    pub fn slot_count(&self) -> usize {
        match self {
            Self::Leaf { .. } => 1,
            Self::Split { a, b, .. } => a.slot_count() + b.slot_count(),
        }
    }

    /// The first slot in left-to-right (top-to-bottom) order — the slot
    /// the session's initial pane (from `new_session`) holds when the plan
    /// is instantiated. The interpreter spawns this slot's shell first,
    /// then splits outward to build the rest of the tree.
    #[must_use]
    pub fn leftmost_slot(&self) -> PaneSlot {
        match self {
            Self::Leaf { slot } => *slot,
            Self::Split { a, .. } => a.leftmost_slot(),
        }
    }

    /// Structural validation: no duplicate slot, every ratio in
    /// `(0.0, 1.0)`. A plan that validates is safe to `realize`.
    pub fn validate(&self) -> Result<(), PlanError> {
        let mut seen = Vec::new();
        self.validate_into(&mut seen)
    }

    fn validate_into(&self, seen: &mut Vec<PaneSlot>) -> Result<(), PlanError> {
        match self {
            Self::Leaf { slot } => {
                if seen.contains(slot) {
                    return Err(PlanError::DuplicateSlot(*slot));
                }
                seen.push(*slot);
                Ok(())
            }
            Self::Split { ratio, a, b, .. } => {
                if !(*ratio > 0.0 && *ratio < 1.0) {
                    return Err(PlanError::BadRatio(*ratio));
                }
                a.validate_into(seen)?;
                b.validate_into(seen)
            }
        }
    }

    /// Turn a plan into a live [`LayoutNode`] by minting one [`PaneId`]
    /// per slot. `mint` is called once per leaf in traversal order — the
    /// instantiation interpreter passes a closure that spawns a PTY and
    /// returns its id. After this, the entire shipped layout algebra
    /// (`compute_rects`, `neighbor`, `split_leaf`, `validate`, …) applies
    /// to the result — the plan reuses the live tree, never duplicates it.
    pub fn realize(&self, mint: &mut impl FnMut(PaneSlot) -> PaneId) -> LayoutNode {
        match self {
            Self::Leaf { slot } => LayoutNode::leaf(mint(*slot)),
            Self::Split {
                orientation,
                ratio,
                a,
                b,
            } => LayoutNode::Split {
                orientation: *orientation,
                ratio: *ratio,
                a: Box::new(a.realize(mint)),
                b: Box::new(b.realize(mint)),
            },
        }
    }

    /// Harvest a *live* [`LayoutNode`] back into a plan — the inverse of
    /// [`realize`](Self::realize). Assigns `PaneSlot(0)`, `(1)`, … to the
    /// tree's leaves in traversal order, producing a structurally-identical
    /// plan (same shape + ratios) plus the `slot → PaneId` map recording
    /// which live pane each slot stood for. This is how a running layout is
    /// captured as a reusable preset: `from_node` dehydrates the live tree,
    /// the plan + spawn specs become a [`crate::SessionDefinition`].
    ///
    /// Round-trips with `realize` both ways: `from_node(realize(plan))`
    /// returns `plan` (when its slots are already `0..n` in traversal
    /// order), and `realize(from_node(node).0, |s| map[&s])` returns `node`.
    #[must_use]
    pub fn from_node(node: &LayoutNode) -> (LayoutPlan, BTreeMap<PaneSlot, PaneId>) {
        let mut next = 0u32;
        let mut map = BTreeMap::new();
        let plan = Self::from_node_into(node, &mut next, &mut map);
        (plan, map)
    }

    /// The running-counter harvest behind [`from_node`](Self::from_node):
    /// assign slots starting at `*next`, advancing it, and record each
    /// `slot → PaneId` in `map`. Use this to harvest *several* trees into
    /// one global slot space — e.g. a multi-window session, where each
    /// window's panes must get distinct slots (per-window `from_node`
    /// would restart at 0 and collide).
    pub fn from_node_into(
        node: &LayoutNode,
        next: &mut u32,
        map: &mut BTreeMap<PaneSlot, PaneId>,
    ) -> LayoutPlan {
        match node {
            LayoutNode::Leaf { pane } => {
                let slot = PaneSlot(*next);
                *next += 1;
                map.insert(slot, *pane);
                LayoutPlan::Leaf { slot }
            }
            LayoutNode::Split {
                orientation,
                ratio,
                a,
                b,
            } => LayoutPlan::Split {
                orientation: *orientation,
                ratio: *ratio,
                a: Box::new(Self::from_node_into(a, next, map)),
                b: Box::new(Self::from_node_into(b, next, map)),
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::geometry::Rect;

    #[test]
    fn slots_traverse_left_to_right() {
        let p = LayoutPlan::split(
            SplitOrientation::Horizontal,
            LayoutPlan::leaf(PaneSlot(0)),
            LayoutPlan::split(
                SplitOrientation::Vertical,
                LayoutPlan::leaf(PaneSlot(1)),
                LayoutPlan::leaf(PaneSlot(2)),
            ),
        );
        assert_eq!(p.slots(), vec![PaneSlot(0), PaneSlot(1), PaneSlot(2)]);
        assert_eq!(p.slot_count(), 3);
    }

    #[test]
    fn validate_rejects_duplicate_slot() {
        let p = LayoutPlan::split(
            SplitOrientation::Vertical,
            LayoutPlan::leaf(PaneSlot(7)),
            LayoutPlan::leaf(PaneSlot(7)),
        );
        assert_eq!(p.validate(), Err(PlanError::DuplicateSlot(PaneSlot(7))));
    }

    #[test]
    fn validate_rejects_degenerate_ratio() {
        let p = LayoutPlan::Split {
            orientation: SplitOrientation::Vertical,
            ratio: 1.0,
            a: Box::new(LayoutPlan::leaf(PaneSlot(0))),
            b: Box::new(LayoutPlan::leaf(PaneSlot(1))),
        };
        assert_eq!(p.validate(), Err(PlanError::BadRatio(1.0)));
    }

    #[test]
    fn realize_maps_slots_to_minted_panes_and_the_live_algebra_applies() {
        // The whole point: a plan realizes into a real LayoutNode that the
        // shipped algebra renders. Mint slot N -> PaneId(100+N).
        let plan = LayoutPlan::split(
            SplitOrientation::Vertical,
            LayoutPlan::leaf(PaneSlot(0)),
            LayoutPlan::leaf(PaneSlot(1)),
        );
        let mut mint = |s: PaneSlot| PaneId(100 + u64::from(s.0));
        let live = plan.realize(&mut mint);
        // It IS a LayoutNode — the shipped algebra works on it.
        assert_eq!(live.panes(), vec![PaneId(100), PaneId(101)]);
        live.validate().unwrap();
        let rects = live.compute_rects(Rect::sized(80, 24));
        let total: u32 = rects.iter().map(|(_, r)| r.area()).sum();
        assert_eq!(total, 80 * 24); // tiles exactly
    }

    #[test]
    fn realize_preserves_ratio_and_orientation() {
        let plan = LayoutPlan::Split {
            orientation: SplitOrientation::Horizontal,
            ratio: 0.25,
            a: Box::new(LayoutPlan::leaf(PaneSlot(0))),
            b: Box::new(LayoutPlan::leaf(PaneSlot(1))),
        };
        let mut mint = |s: PaneSlot| PaneId(u64::from(s.0));
        match plan.realize(&mut mint) {
            LayoutNode::Split { orientation, ratio, .. } => {
                assert_eq!(orientation, SplitOrientation::Horizontal);
                assert!((ratio - 0.25).abs() < f32::EPSILON);
            }
            LayoutNode::Leaf { .. } => panic!("expected a split"),
        }
    }

    #[test]
    fn spawn_spec_serde_round_trips() {
        let s = SpawnSpec::shell(PaneSlot(3), "/bin/zsh");
        let json = serde_json::to_string(&s).unwrap();
        let back: SpawnSpec = serde_json::from_str(&json).unwrap();
        assert_eq!(s, back);
    }

    fn sample_plan() -> LayoutPlan {
        // slot 0 | (slot 1 / slot 2), asymmetric ratios.
        LayoutPlan::Split {
            orientation: SplitOrientation::Vertical,
            ratio: 0.3,
            a: Box::new(LayoutPlan::leaf(PaneSlot(0))),
            b: Box::new(LayoutPlan::Split {
                orientation: SplitOrientation::Horizontal,
                ratio: 0.7,
                a: Box::new(LayoutPlan::leaf(PaneSlot(1))),
                b: Box::new(LayoutPlan::leaf(PaneSlot(2))),
            }),
        }
    }

    #[test]
    fn from_node_dehydrates_a_live_tree_with_canonical_slots() {
        // realize the plan, then harvest it back. Slots come out 0,1,2 in
        // traversal order; the map records the minted panes.
        let plan = sample_plan();
        let mut mint = |s: PaneSlot| PaneId(100 + u64::from(s.0));
        let node = plan.realize(&mut mint);
        let (plan2, map) = LayoutPlan::from_node(&node);
        // Structure + ratios round-trip; slots are already canonical here.
        assert_eq!(plan2, plan);
        assert_eq!(map[&PaneSlot(0)], PaneId(100));
        assert_eq!(map[&PaneSlot(1)], PaneId(101));
        assert_eq!(map[&PaneSlot(2)], PaneId(102));
    }

    #[test]
    fn realize_of_from_node_reconstructs_the_original_live_tree() {
        // The other direction: harvest an arbitrary live tree, then realize
        // the plan back through the recorded map — exactly the input tree.
        let node = LayoutNode::Split {
            orientation: SplitOrientation::Horizontal,
            ratio: 0.4,
            a: Box::new(LayoutNode::leaf(PaneId(7))),
            b: Box::new(LayoutNode::Split {
                orientation: SplitOrientation::Vertical,
                ratio: 0.6,
                a: Box::new(LayoutNode::leaf(PaneId(9))),
                b: Box::new(LayoutNode::leaf(PaneId(2))),
            }),
        };
        let (plan, map) = LayoutPlan::from_node(&node);
        let mut mint = |s: PaneSlot| map[&s];
        assert_eq!(plan.realize(&mut mint), node);
        // The plan carries no live id — it references only slots.
        assert_eq!(plan.slots(), vec![PaneSlot(0), PaneSlot(1), PaneSlot(2)]);
    }

    #[test]
    fn from_node_of_a_single_pane_is_one_slot() {
        let (plan, map) = LayoutPlan::from_node(&LayoutNode::leaf(PaneId(42)));
        assert_eq!(plan, LayoutPlan::leaf(PaneSlot(0)));
        assert_eq!(map[&PaneSlot(0)], PaneId(42));
    }
}
