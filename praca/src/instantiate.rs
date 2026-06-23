//! The instantiation interpreter — the typed morphism that turns a
//! latent [`SessionDefinition`] into a live [`LiveSession`].
//!
//! This is leg 3 of the (defsession) triplet (border = the types in
//! [`crate::definition`] + tear-types; authoring = the future
//! `(defsession)` form; interpreter = here). It is also the constructive
//! answer to two pressure-test illegal states:
//!
//! * #5 — "no way to instantiate a definition": [`instantiate`] IS that
//!   typed operation, and [`crate::attach::AttachAction::Instantiate`] is
//!   its decision branch.
//! * #6 — "PTYs survive a daemon restart": [`reinstantiate`] makes the
//!   correct model constructive. A restart does not resurrect dead
//!   processes (no value expresses that); it re-runs `instantiate`
//!   against the surviving definition, producing a FRESH incarnation with
//!   a fresh [`tear_types::InstanceId`] and fresh PTYs.
//!
//! The interpreter drives any [`MultiplexerControl`] — it takes `&dyn`,
//! so a test double or the real `tear-core::InProcess` are interchangeable
//! (the Environment-trait testability contract). It reuses the SHIPPED
//! backend ops (`new_session…`, `new_window`, `split_pane`, `select_pane`)
//! to rebuild the plan's tree: spawn the leftmost slot's shell, then split
//! outward so every leaf lands on its slot's pane with its slot's shell.
//!
//! Tier-honest (M1): the tree *shape* and per-pane *shell* are exact; the
//! split *ratios* default to the backend's balanced split (the plan's
//! ratios are not yet applied — that needs a ratio-bearing split op or a
//! post-spawn resize pass, a named follow-up). Per-pane `cwd`/`env`/`args`
//! beyond the shell likewise await a richer backend spawn op.

use std::collections::BTreeMap;
use std::fmt;

use tear_types::{
    ControlError, Direction, LayoutPlan, LiveSession, MultiplexerControl, PaneId, PaneSlot,
    SessionSource, SplitOrientation, WindowId,
};

use crate::definition::{DefinitionError, SessionDefinition, SessionOrigin};

/// Why [`instantiate`] failed.
#[derive(Debug)]
pub enum InstantiateError {
    /// The definition did not validate — caught before any spawn, so a
    /// malformed definition never half-builds a session.
    Invalid(DefinitionError),
    /// A backend operation failed mid-build.
    Control(ControlError),
}

impl fmt::Display for InstantiateError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Invalid(e) => write!(f, "invalid session definition: {e:?}"),
            Self::Control(e) => write!(f, "backend error during instantiation: {e}"),
        }
    }
}

impl std::error::Error for InstantiateError {}

impl From<ControlError> for InstantiateError {
    fn from(e: ControlError) -> Self {
        Self::Control(e)
    }
}

/// The runtime provenance an instantiation carries. The definition's
/// [`SessionOrigin`] is *where the definition was born*; the session
/// source is *who/what is running it now*. For M1 both project and ad-hoc
/// definitions instantiate as `Human`; authored blueprints as a `Named`
/// automation tag.
fn source_for(origin: &SessionOrigin) -> SessionSource {
    match origin {
        SessionOrigin::Project | SessionOrigin::Adhoc { .. } => SessionSource::Human,
        SessionOrigin::Authored => SessionSource::Named("defsession".into()),
    }
}

/// The backend split direction that places the new (b-side) pane for a
/// split of the given orientation: a side-by-side (Vertical) split puts
/// the new pane to the Right; a stacked (Horizontal) split puts it Below.
fn split_direction(orientation: SplitOrientation) -> Direction {
    match orientation {
        SplitOrientation::Vertical => Direction::Right,
        SplitOrientation::Horizontal => Direction::Below,
    }
}

/// Reshape the already-spawned `occupant` pane into `plan`'s tree, by
/// splitting outward and spawning each additional pane with its slot's
/// shell. Records every slot→PaneId mapping in `out`.
fn realize_into(
    plan: &LayoutPlan,
    occupant: PaneId,
    specs: &BTreeMap<PaneSlot, tear_types::SpawnSpec>,
    backend: &dyn MultiplexerControl,
    out: &mut BTreeMap<PaneSlot, PaneId>,
) -> Result<(), InstantiateError> {
    match plan {
        LayoutPlan::Leaf { slot } => {
            out.insert(*slot, occupant);
            Ok(())
        }
        LayoutPlan::Split {
            orientation, a, b, ..
        } => {
            // The new pane takes the b-side; spawn it with b's leftmost
            // slot's shell. `validate()` guaranteed every slot has a spec.
            let b_first = b.leftmost_slot();
            let b_shell = specs[&b_first].shell.clone();
            let new_pane = backend.split_pane(occupant, split_direction(*orientation), &b_shell)?;
            // `occupant` stays as side a; recurse into both halves.
            realize_into(a, occupant, specs, backend, out)?;
            realize_into(b, new_pane, specs, backend, out)?;
            Ok(())
        }
    }
}

/// Realize one window plan against `backend`, returning its window id and
/// the slot→pane map. `first` is true for a definition's first window
/// (created by `new_session`, already done by the caller — `wid` is that
/// window); otherwise a fresh window is created here.
fn realize_window(
    window: &tear_types::WindowPlan,
    wid: WindowId,
    specs: &BTreeMap<PaneSlot, tear_types::SpawnSpec>,
    backend: &dyn MultiplexerControl,
) -> Result<(), InstantiateError> {
    // The first pane the backend spawned for this window is the occupant
    // of the whole plan — it holds the leftmost slot.
    let first_pane = backend.get_window(wid)?.1.active_pane;
    let mut slot_to_pane = BTreeMap::new();
    realize_into(&window.layout, first_pane, specs, backend, &mut slot_to_pane)?;
    // Focus the window's declared active slot.
    if let Some(&active) = slot_to_pane.get(&window.active_slot) {
        backend.select_pane(active)?;
    }
    Ok(())
}

/// Instantiate a definition into a live session against `backend`. Builds
/// every window (first via `new_session`, the rest via `new_window`) and
/// every window's pane tree, then returns the [`LiveSession`] linking the
/// fresh incarnation back to its [`DefinitionId`].
pub fn instantiate(
    def: &SessionDefinition,
    backend: &dyn MultiplexerControl,
) -> Result<LiveSession, InstantiateError> {
    def.validate().map_err(InstantiateError::Invalid)?;
    let source = source_for(&def.origin);
    let display = def.display_name();

    let mut session_id = None;
    let mut first_window = None;
    for (i, window) in def.windows.iter().enumerate() {
        let leftmost = window.layout.leftmost_slot();
        let first_shell = def.pane_specs[&leftmost].shell.clone();
        let wid = if i == 0 {
            let sid = backend.new_session_with_source_and_size(
                &display,
                &first_shell,
                source.clone(),
                (80, 24),
            )?;
            session_id = Some(sid);
            let w = backend.get_session(sid)?.active_window;
            first_window = Some(w);
            w
        } else {
            backend.new_window(session_id.expect("first window set"), &window.name, &first_shell)?
        };
        realize_window(window, wid, &def.pane_specs, backend)?;
    }

    // Open the session on its first window (later new_window calls may
    // have moved the active window).
    if let Some(w) = first_window {
        backend.select_window(w)?;
    }

    let sid = session_id.expect("a validated definition has ≥1 window");
    let session = backend.get_session(sid)?;
    Ok(LiveSession::new(def.def_id, session))
}

/// Re-instantiate a definition — the constructive meaning of "restart the
/// session". A daemon restart cannot resurrect the dead PTYs (no
/// [`tear_types::Durability`] value expresses survival); recovery is to
/// run [`instantiate`] again against the surviving definition, producing a
/// fresh incarnation (new [`tear_types::InstanceId`], new processes) of
/// the SAME definition. It is exactly `instantiate` — the distinct name
/// documents the intent at call sites.
pub fn reinstantiate(
    def: &SessionDefinition,
    backend: &dyn MultiplexerControl,
) -> Result<LiveSession, InstantiateError> {
    instantiate(def, backend)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::record::NameStyle;
    use tear_core::inproc::InProcess;
    use tear_types::WindowPlan;

    fn specs(pairs: &[(u32, &str)]) -> BTreeMap<PaneSlot, tear_types::SpawnSpec> {
        pairs
            .iter()
            .map(|&(s, sh)| (PaneSlot(s), tear_types::SpawnSpec::shell(PaneSlot(s), sh)))
            .collect()
    }

    #[test]
    fn instantiate_single_pane_definition_spawns_one_pane_linked_to_its_def() {
        let backend = InProcess::new();
        let def = SessionDefinition::single_pane("/code/pleme-io/mado", "/bin/sh", NameStyle::Emoji, 0);
        let live = instantiate(&def, &backend).unwrap();
        // The live session links back to the definition (illegal state #1/#5).
        assert_eq!(live.definition, def.def_id);
        // One window, one pane, process-bound (illegal state #6).
        let w = &live.session.windows[&live.session.active_window];
        assert_eq!(w.layout.pane_count(), 1);
        assert_eq!(live.durability, tear_types::Durability::ProcessBound);
    }

    #[test]
    fn instantiate_two_pane_plan_builds_the_exact_tree() {
        let backend = InProcess::new();
        // slot 0 | slot 1, side by side.
        let def = SessionDefinition {
            def_id: tear_types::DefinitionId::from_project(std::path::Path::new("/x")),
            origin: SessionOrigin::Project,
            name_seed: 0,
            name_style: NameStyle::Emoji,
            theme: None,
            custom_name: None,
            project_root: "/x".into(),
            windows: vec![WindowPlan {
                name: "work".into(),
                layout: LayoutPlan::split(
                    SplitOrientation::Vertical,
                    LayoutPlan::leaf(PaneSlot(0)),
                    LayoutPlan::leaf(PaneSlot(1)),
                ),
                active_slot: PaneSlot(1),
            }],
            pane_specs: specs(&[(0, "/bin/sh"), (1, "/bin/sh")]),
            visits: 1,
            last_seen: 0,
            tags: Vec::new(),
        };
        let live = instantiate(&def, &backend).unwrap();
        let w = &live.session.windows[&live.session.active_window];
        // Two panes in a single vertical split — the plan's exact shape.
        assert_eq!(w.layout.pane_count(), 2);
        match &w.layout {
            tear_types::LayoutNode::Split { orientation, .. } => {
                assert_eq!(*orientation, SplitOrientation::Vertical);
            }
            tear_types::LayoutNode::Leaf { .. } => panic!("expected a split"),
        }
        // The active slot (1) is focused → it is the rightmost leaf.
        let panes = w.layout.panes();
        assert_eq!(w.active_pane, panes[1]);
        live.session
            .windows
            .values()
            .for_each(|win| win.layout.validate().unwrap());
    }

    #[test]
    fn instantiate_nested_three_pane_plan_matches_structure() {
        let backend = InProcess::new();
        // slot 0 | (slot 1 / slot 2)
        let def = SessionDefinition {
            def_id: tear_types::DefinitionId::from_project(std::path::Path::new("/y")),
            origin: SessionOrigin::Project,
            name_seed: 0,
            name_style: NameStyle::Emoji,
            theme: None,
            custom_name: None,
            project_root: "/y".into(),
            windows: vec![WindowPlan {
                name: "work".into(),
                layout: LayoutPlan::split(
                    SplitOrientation::Vertical,
                    LayoutPlan::leaf(PaneSlot(0)),
                    LayoutPlan::split(
                        SplitOrientation::Horizontal,
                        LayoutPlan::leaf(PaneSlot(1)),
                        LayoutPlan::leaf(PaneSlot(2)),
                    ),
                ),
                active_slot: PaneSlot(0),
            }],
            pane_specs: specs(&[(0, "/bin/sh"), (1, "/bin/sh"), (2, "/bin/sh")]),
            visits: 1,
            last_seen: 0,
            tags: Vec::new(),
        };
        let live = instantiate(&def, &backend).unwrap();
        let w = &live.session.windows[&live.session.active_window];
        assert_eq!(w.layout.pane_count(), 3);
        // Outer split is vertical, its b-side is a horizontal split.
        match &w.layout {
            tear_types::LayoutNode::Split { orientation, b, .. } => {
                assert_eq!(*orientation, SplitOrientation::Vertical);
                assert!(matches!(
                    b.as_ref(),
                    tear_types::LayoutNode::Split { orientation: SplitOrientation::Horizontal, .. }
                ));
            }
            tear_types::LayoutNode::Leaf { .. } => panic!("expected a split"),
        }
        w.layout.validate().unwrap();
    }

    #[test]
    fn reinstantiate_yields_a_fresh_incarnation_of_the_same_definition() {
        let backend = InProcess::new();
        let def = SessionDefinition::single_pane("/code/pleme-io/tear", "/bin/sh", NameStyle::Emoji, 0);
        let first = instantiate(&def, &backend).unwrap();
        let second = reinstantiate(&def, &backend).unwrap();
        // Same definition…
        assert_eq!(first.definition, second.definition);
        // …but DISTINCT live incarnations (fresh InstanceId) — restart is
        // re-instantiation, not resurrection (illegal state #6).
        assert_ne!(first.instance(), second.instance());
    }

    /// A `format!`-free structural fingerprint of a layout tree:
    /// orientations + shape, ignoring exact `PaneId`s and ratios.
    fn shape(n: &tear_types::LayoutNode, out: &mut String) {
        match n {
            tear_types::LayoutNode::Leaf { .. } => out.push('L'),
            tear_types::LayoutNode::Split { orientation, a, b, .. } => {
                out.push('(');
                out.push(if *orientation == SplitOrientation::Vertical {
                    'V'
                } else {
                    'H'
                });
                shape(a, out);
                shape(b, out);
                out.push(')');
            }
        }
    }

    #[test]
    fn capture_then_instantiate_reproduces_the_layout_structure() {
        use crate::definition::SessionDefinition;
        // Original: a 3-pane shape p0 | (p1 / p2).
        let backend = InProcess::new();
        let sid = backend.new_session("orig", "/bin/sh").unwrap();
        let w = backend.get_session(sid).unwrap().active_window;
        let p0 = backend.get_session(sid).unwrap().windows[&w].active_pane;
        let p1 = backend.split_pane(p0, Direction::Right, "/bin/sh").unwrap();
        backend.split_pane(p1, Direction::Below, "/bin/sh").unwrap();
        let original = backend.get_session(sid).unwrap();
        let mut orig_shape = String::new();
        shape(&original.windows[&w].layout, &mut orig_shape);

        // Capture → definition → instantiate into a FRESH backend.
        let def = SessionDefinition::from_live(&original, "/code/x", 0);
        let backend2 = InProcess::new();
        let live = instantiate(&def, &backend2).unwrap();
        let new_w = &live.session.windows[&live.session.active_window];
        let mut new_shape = String::new();
        shape(&new_w.layout, &mut new_shape);

        // The structural fingerprint round-trips (orientations + tree shape).
        assert_eq!(new_shape, orig_shape, "captured layout structure differs");
        assert_eq!(new_w.layout.pane_count(), 3);
    }

    #[test]
    fn instantiate_rejects_invalid_definition_before_spawning() {
        let backend = InProcess::new();
        let mut def = SessionDefinition::single_pane("/x", "/bin/sh", NameStyle::Emoji, 0);
        // Reference a slot with no spec → validate() fails → no spawn.
        def.windows.push(WindowPlan::single("orphan", PaneSlot(42)));
        let err = instantiate(&def, &backend).unwrap_err();
        assert!(matches!(err, InstantiateError::Invalid(DefinitionError::MissingSpec(_))));
        // Nothing was spawned.
        assert!(backend.list_sessions().unwrap().is_empty());
    }
}
