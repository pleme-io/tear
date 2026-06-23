//! The latent half of the session model: a [`SessionDefinition`] — the
//! durable *shape* of a session, from which live incarnations are
//! instantiated.
//!
//! This is the typed payload the pressure-test found missing. Before, a
//! `SessionRecord` stored only metadata (no layout, no panes, no
//! commands), [`crate::record::SessionState::Templated`] was an empty
//! enum arm with nothing behind it, and the ad-hoc construction path was
//! a silent third way to make a session. Here:
//!
//! * a definition CARRIES its plan — [`tear_types::WindowPlan`]s over
//!   [`tear_types::PaneSlot`]s + a [`tear_types::SpawnSpec`] per slot — so
//!   there is always something to instantiate (illegal state #3/#4);
//! * its identity is the durable [`DefinitionId`] (illegal state #1), and
//! * its provenance is a typed [`SessionOrigin`], not a hidden code path
//!   (illegal state #9).
//!
//! The interpreter that turns a definition into a [`tear_types::LiveSession`]
//! lives in [`crate::instantiate`] (M1b).

use std::collections::BTreeMap;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use tear_types::{DefinitionId, PaneSlot, PlanError, SpawnSpec, WindowPlan};

use crate::record::{display_name_for, NameStyle, ThemeMirror};

/// Where a session definition came from — a typed third arm, so the
/// ad-hoc path is exhaustively matched rather than a silent `for_adhoc`
/// branch (pressure-test illegal state #9).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum SessionOrigin {
    /// Born from `cd`-ing into a project — the path-stable case. The
    /// definition's identity is the project root's stable seed.
    Project,
    /// An ad-hoc themed session (the `for_adhoc` path): a random name
    /// drawn from one theme register, keyed on `seed`.
    Adhoc {
        /// The theme register the random name is drawn from.
        theme: ThemeMirror,
        /// The seed the random name + identity derive from.
        seed: u64,
    },
    /// Authored from a `(defsession …)` blueprint — the triplet's
    /// authoring leg (lands when the `(defsession)` parser does).
    Authored,
}

/// Why a [`SessionDefinition`] failed [`SessionDefinition::validate`].
/// Every variant names a definition that could not be instantiated
/// faithfully, so a malformed definition is caught before it spawns
/// anything.
#[derive(Clone, Debug, PartialEq)]
pub enum DefinitionError {
    /// The definition has no windows — nothing to instantiate.
    NoWindows,
    /// One window's layout plan is structurally invalid.
    Layout(PlanError),
    /// A slot in some window's layout has no [`SpawnSpec`] — the
    /// interpreter would not know what to spawn there.
    MissingSpec(PaneSlot),
    /// A window's `active_slot` is not one of that window's layout slots.
    ActiveSlotNotInWindow(PaneSlot),
}

/// The durable shape of a session: identity, provenance, naming, the
/// project it belongs to, and the PLAN (windows + per-pane spawn specs)
/// the interpreter realizes into live panes.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SessionDefinition {
    /// Durable, project-stable identity (illegal state #1). Equal to the
    /// project's `name_seed` for `Project`-origin definitions.
    pub def_id: DefinitionId,
    /// Typed provenance (illegal state #9).
    pub origin: SessionOrigin,
    /// Atlas seed for the display name (mirrors `SessionRecord.name_seed`).
    pub name_seed: u64,
    /// Render style for the name.
    pub name_style: NameStyle,
    /// Theme the random name is drawn from (`None` = whole-pool).
    #[serde(default)]
    pub theme: Option<ThemeMirror>,
    /// Operator-chosen name overriding the emoji identity.
    #[serde(default)]
    pub custom_name: Option<String>,
    /// The project this definition belongs to.
    pub project_root: PathBuf,
    /// The window plans — at least one (enforced by [`Self::validate`]).
    pub windows: Vec<WindowPlan>,
    /// The spawn spec for every slot referenced by the window plans.
    pub pane_specs: BTreeMap<PaneSlot, SpawnSpec>,
    /// Frecency: total visits (mirrors the record's field).
    #[serde(default)]
    pub visits: u32,
    /// Frecency: unix-seconds of last touch (injected, never clock-read).
    #[serde(default)]
    pub last_seen: u64,
    /// Free-form operator tags.
    #[serde(default)]
    pub tags: Vec<String>,
}

impl SessionDefinition {
    /// A single-window, single-pane project definition running `shell` —
    /// the common case (most sessions start as one shell in one project).
    /// The lone slot is `PaneSlot(0)`; identity is the project's stable seed.
    #[must_use]
    pub fn single_pane(
        project_root: impl Into<PathBuf>,
        shell: impl Into<String>,
        name_style: NameStyle,
        last_seen: u64,
    ) -> Self {
        let project_root = project_root.into();
        let def_id = DefinitionId::from_project(&project_root);
        let slot = PaneSlot(0);
        let mut pane_specs = BTreeMap::new();
        pane_specs.insert(slot, SpawnSpec::shell(slot, shell));
        Self {
            def_id,
            origin: SessionOrigin::Project,
            name_seed: def_id.0,
            name_style,
            theme: None,
            custom_name: None,
            project_root,
            windows: vec![WindowPlan::single("main", slot)],
            pane_specs,
            visits: 1,
            last_seen,
            tags: Vec::new(),
        }
    }

    /// The session's display name (custom name, else the themed emoji
    /// name). Shares the resolver with [`crate::record::SessionRecord`].
    #[must_use]
    pub fn display_name(&self) -> String {
        display_name_for(
            self.name_seed,
            self.name_style,
            self.theme,
            self.custom_name.as_deref(),
        )
    }

    /// Structural validation: ≥1 window, every window's layout valid,
    /// every referenced slot has a spawn spec, every `active_slot` belongs
    /// to its window. A definition that validates can be instantiated.
    pub fn validate(&self) -> Result<(), DefinitionError> {
        if self.windows.is_empty() {
            return Err(DefinitionError::NoWindows);
        }
        for w in &self.windows {
            w.layout.validate().map_err(DefinitionError::Layout)?;
            let slots = w.layout.slots();
            for slot in &slots {
                if !self.pane_specs.contains_key(slot) {
                    return Err(DefinitionError::MissingSpec(*slot));
                }
            }
            if !slots.contains(&w.active_slot) {
                return Err(DefinitionError::ActiveSlotNotInWindow(w.active_slot));
            }
        }
        Ok(())
    }

    /// Total slots across all windows.
    #[must_use]
    pub fn slot_count(&self) -> usize {
        self.windows.iter().map(|w| w.layout.slot_count()).sum()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tear_types::{LayoutPlan, SplitOrientation};

    #[test]
    fn single_pane_definition_validates() {
        let d = SessionDefinition::single_pane("/code/pleme-io/mado", "/bin/zsh", NameStyle::Emoji, 0);
        d.validate().unwrap();
        assert_eq!(d.slot_count(), 1);
        // Identity is the project seed (illegal state #1) — matches the id
        // a SessionRecord for the same project would carry.
        assert_eq!(d.def_id, DefinitionId::from_project(std::path::Path::new("/code/pleme-io/mado")));
        assert_eq!(d.name_seed, d.def_id.0);
    }

    #[test]
    fn validate_rejects_slot_without_spec() {
        let mut d = SessionDefinition::single_pane("/x", "/bin/zsh", NameStyle::Emoji, 0);
        // Add a window referencing slot 9 with no spec.
        d.windows.push(WindowPlan::single("orphan", PaneSlot(9)));
        assert_eq!(d.validate(), Err(DefinitionError::MissingSpec(PaneSlot(9))));
    }

    #[test]
    fn validate_rejects_active_slot_not_in_window() {
        let mut d = SessionDefinition::single_pane("/x", "/bin/zsh", NameStyle::Emoji, 0);
        d.pane_specs.insert(PaneSlot(1), SpawnSpec::shell(PaneSlot(1), "/bin/sh"));
        // Window over slot 0, but active_slot points at 1 (not in it).
        d.windows[0].active_slot = PaneSlot(1);
        assert_eq!(d.validate(), Err(DefinitionError::ActiveSlotNotInWindow(PaneSlot(1))));
    }

    #[test]
    fn multi_pane_definition_validates() {
        // Two-pane window: slot 0 | slot 1.
        let mut pane_specs = BTreeMap::new();
        pane_specs.insert(PaneSlot(0), SpawnSpec::shell(PaneSlot(0), "/bin/zsh"));
        pane_specs.insert(PaneSlot(1), SpawnSpec::shell(PaneSlot(1), "/bin/sh"));
        let d = SessionDefinition {
            def_id: DefinitionId::from_project(std::path::Path::new("/x")),
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
                active_slot: PaneSlot(0),
            }],
            pane_specs,
            visits: 1,
            last_seen: 0,
            tags: Vec::new(),
        };
        d.validate().unwrap();
        assert_eq!(d.slot_count(), 2);
    }

    #[test]
    fn origin_adhoc_is_a_typed_arm() {
        // The ad-hoc path is now an exhaustively-matchable value.
        let o = SessionOrigin::Adhoc { theme: ThemeMirror::Brazil, seed: 42 };
        match o {
            SessionOrigin::Project | SessionOrigin::Authored => panic!("wrong arm"),
            SessionOrigin::Adhoc { theme, seed } => {
                assert_eq!(theme, ThemeMirror::Brazil);
                assert_eq!(seed, 42);
            }
        }
    }

    #[test]
    fn definition_serde_round_trips() {
        let d = SessionDefinition::single_pane("/x", "/bin/zsh", NameStyle::Emoji, 7);
        let json = serde_json::to_string(&d).unwrap();
        let back: SessionDefinition = serde_json::from_str(&json).unwrap();
        assert_eq!(d, back);
    }
}
