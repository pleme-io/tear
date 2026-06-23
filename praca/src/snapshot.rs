//! [`PracaSnapshot`] — the serde-friendly, persistable projection of a
//! [`crate::Praca`] facade.
//!
//! The live `Praca` holds two foreign enums that are deliberately NOT
//! serde-friendly: [`crate::AttachPolicy`] (a pure-logic enum that never
//! needed serialisation in the M0 substrate) and
//! `ishou_tokens::SessionNameStyle` (whose serde the substrate doesn't
//! own). Rather than bolt serde onto either, the snapshot mirrors them
//! with local serde enums — [`PolicyMirror`] for the policy and the
//! already-serde [`crate::NameStyle`] for the style — and stores the
//! [`SessionIndex`] + [`ProjectBinding`] verbatim (both already serde).
//!
//! The daemon (M1 "Remember") writes a `PracaSnapshot` to its state file
//! on every lifecycle mutation and reloads it at startup, so project↔
//! session bindings and frecency counters survive a restart.

use serde::{Deserialize, Serialize};

use crate::index::SessionIndex;
use crate::AttachPolicy;
use crate::DefinitionIndex;
use crate::NameStyle;
use crate::ProjectBinding;

/// Serde-friendly mirror of [`AttachPolicy`] (the attach enum is a
/// pure-logic type with no serde derive). Converts losslessly both ways.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PolicyMirror {
    /// `cd` across projects auto-attaches — the automation-first default.
    #[default]
    AutoSwitch,
    /// Compute the switch/spawn but surface it as a non-forced hint.
    SuggestOnly,
    /// Automation fully off — the operator drives via the picker.
    PickerOnly,
}

impl From<AttachPolicy> for PolicyMirror {
    fn from(p: AttachPolicy) -> Self {
        match p {
            AttachPolicy::AutoSwitch => PolicyMirror::AutoSwitch,
            AttachPolicy::SuggestOnly => PolicyMirror::SuggestOnly,
            AttachPolicy::PickerOnly => PolicyMirror::PickerOnly,
        }
    }
}

impl From<PolicyMirror> for AttachPolicy {
    fn from(p: PolicyMirror) -> Self {
        match p {
            PolicyMirror::AutoSwitch => AttachPolicy::AutoSwitch,
            PolicyMirror::SuggestOnly => AttachPolicy::SuggestOnly,
            PolicyMirror::PickerOnly => AttachPolicy::PickerOnly,
        }
    }
}

/// The persistable projection of a [`crate::Praca`] facade.
///
/// Round-trips byte-for-byte through `serde_json` — the daemon's on-disk
/// praça store. `index` carries every [`crate::SessionRecord`] (with its
/// frecency counters); `binding` carries the project-root → session map;
/// `policy` + `name_style` carry the automation configuration.
// No `Eq` — the persisted `definitions` catalog carries `SessionDefinition`s
// with an `f32` split ratio (`PartialEq` only).
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct PracaSnapshot {
    /// Searchable catalog of every tracked session, with frecency.
    #[serde(default)]
    pub index: SessionIndex,
    /// Persisted project-root → session bindings (the cd-attach memory).
    #[serde(default)]
    pub binding: ProjectBinding,
    /// Persisted latent-preset catalog — saved/authored definitions that
    /// survive a restart (so a preset stays launchable across sessions).
    /// `#[serde(default)]` keeps older snapshots (without this field)
    /// loadable: they deserialize to an empty catalog.
    #[serde(default)]
    pub definitions: DefinitionIndex,
    /// Auto-attach aggressiveness.
    #[serde(default)]
    pub policy: PolicyMirror,
    /// Style for auto-generated session names.
    #[serde(default)]
    pub name_style: NameStyle,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Praca, SessionRecord};
    use ishou_tokens::SessionNameStyle;
    use std::path::{Path, PathBuf};
    use tear_types::id::SessionId;

    fn sid(s: &str) -> SessionId {
        SessionId::from_seed(s)
    }

    #[test]
    fn policy_mirror_round_trips_every_variant() {
        for p in [
            AttachPolicy::AutoSwitch,
            AttachPolicy::SuggestOnly,
            AttachPolicy::PickerOnly,
        ] {
            let mirror: PolicyMirror = p.into();
            let back: AttachPolicy = mirror.into();
            assert_eq!(p, back);
        }
    }

    #[test]
    fn praca_snapshot_round_trips_through_json() {
        let mut p = Praca::new();
        let rec = SessionRecord::for_project(
            sid("tide"),
            PathBuf::from("/code/pleme-io/mado"),
            SessionNameStyle::Emoji,
            1_700,
        );
        p.index.upsert(rec);
        p.binding
            .bind(PathBuf::from("/code/pleme-io/mado"), sid("tide"));
        p.record_visit(sid("tide"), 1_999);

        let snap = p.to_snapshot();
        let json = serde_json::to_string(&snap).unwrap();
        let back: PracaSnapshot = serde_json::from_str(&json).unwrap();
        assert_eq!(snap, back);

        // The rebuilt facade resolves the binding + carries the visit.
        let restored = Praca::from_snapshot(back);
        assert_eq!(
            restored.binding.lookup(Path::new("/code/pleme-io/mado")),
            Some(sid("tide"))
        );
        let r = restored.index.get(sid("tide")).unwrap();
        assert_eq!(r.last_seen, 1_999);
        assert_eq!(r.visits, 2); // for_project starts at 1, +1 visit.
    }

    #[test]
    fn snapshot_persists_and_restores_the_latent_preset_catalog() {
        use crate::{NameStyle, SessionDefinition};
        let mut p = Praca::new();
        p.definitions.upsert(SessionDefinition::single_pane(
            "/code/pleme-io/substrate",
            "/bin/zsh",
            NameStyle::Emoji,
            1_000,
        ));
        let snap = p.to_snapshot();
        let json = serde_json::to_string(&snap).unwrap();
        let back: PracaSnapshot = serde_json::from_str(&json).unwrap();
        // The catalog survives the round-trip — a preset is launchable
        // after a restart.
        let restored = Praca::from_snapshot(back);
        assert_eq!(restored.definitions.len(), 1);
        assert!(restored
            .definitions
            .by_project(Path::new("/code/pleme-io/substrate"))
            .is_some());
    }

    #[test]
    fn old_snapshot_without_definitions_loads_empty() {
        // Backward compat: a pre-feature snapshot JSON (no `definitions`
        // field) must still deserialize, defaulting to an empty catalog.
        let snap: PracaSnapshot = serde_json::from_str("{}").expect("empty snapshot loads");
        assert!(snap.definitions.is_empty());
        let p = Praca::from_snapshot(snap);
        assert!(p.definitions.is_empty());
    }
}
