//! The attach engine — the automation core.
//!
//! When the operator `cd`s, praca decides what should happen to the
//! current session: stay put, switch to a session already bound to the
//! destination project, or spawn a fresh auto-named session for a
//! brand-new project. That decision is [`decide`]; the policy that
//! governs whether the decision is *acted on* automatically is
//! [`AttachPolicy`].
//!
//! The whole thing is **pure**: [`decide`] takes the current record, the
//! new cwd, the binding map, and the index by reference and returns a
//! typed [`AttachDecision`]. No I/O, no clock — the caller (mado/tear,
//! later) is responsible for actually performing the switch/spawn and
//! recording the visit.

use std::path::{Path, PathBuf};

use ishou_tokens::{FleetSessionNames, SessionName, SessionNameStyle};
use tear_types::id::SessionId;
use tear_types::{DefinitionId, InstanceId};

use crate::binding::ProjectBinding;
use crate::index::SessionIndex;
use crate::project::project_root;
use crate::record::SessionRecord;

/// What should happen to the operator's current session after a `cd`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AttachDecision {
    /// Do nothing — the operator is still inside the current session's
    /// project (or automation is off).
    Stay,
    /// Switch to an existing session already bound to the destination
    /// project.
    SwitchTo(SessionId),
    /// No session exists for the destination project — spawn a fresh one
    /// at `project_root`, auto-named `name`.
    SpawnNew {
        /// The resolved project root the new session binds to.
        project_root: PathBuf,
        /// The deterministic auto-name for that project.
        name: SessionName,
    },
    /// A non-forced suggestion (under [`AttachPolicy::SuggestOnly`]).
    /// The inner decision is what *would* happen under `AutoSwitch`; the
    /// caller surfaces it (a hint/toast) rather than acting on it.
    Suggest(Box<AttachDecision>),
}

/// The typescaped successor to [`AttachDecision`] — adds the missing
/// `Instantiate` branch (pressure-test illegal state #5) so "realize a
/// known definition" is a first-class typed outcome, and speaks the new
/// two-tier id vocabulary ([`InstanceId`] for live handles,
/// [`DefinitionId`] for durable definitions). M1 ships this alongside the
/// shipped [`AttachDecision`]; the `decide` rewrite onto it is M2.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AttachAction {
    /// Do nothing — still inside the current session's project (or
    /// automation is off).
    Stay,
    /// Switch to an existing *live* instance bound to the destination.
    SwitchTo(InstanceId),
    /// A definition for the destination exists but has no live instance —
    /// instantiate it (run the blueprint, spawn its panes), then attach.
    /// This is the branch [`AttachDecision`] simply could not express.
    Instantiate(DefinitionId),
    /// No definition exists yet for the destination — spawn a fresh
    /// auto-named session, which *creates* the definition.
    SpawnNew {
        /// The resolved project root the new session binds to.
        project_root: PathBuf,
        /// The deterministic auto-name for that project.
        name: SessionName,
    },
}

/// How aggressively the engine acts on a `cd`.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Default)]
pub enum AttachPolicy {
    /// `cd` across projects auto-attaches: the engine returns a real
    /// `SwitchTo` / `SpawnNew` and the caller performs it. The
    /// automation-first default.
    #[default]
    AutoSwitch,
    /// Compute the same Switch/Spawn but wrap it in
    /// [`AttachDecision::Suggest`] — the caller surfaces it as a
    /// non-forced hint and never switches behind the operator's back.
    SuggestOnly,
    /// Automation fully off — always [`AttachDecision::Stay`]. The
    /// operator drives session changes by hand (the picker).
    PickerOnly,
}

/// Decide what to do with the current session after a `cd` to `new_cwd`.
///
/// Logic:
/// 1. [`AttachPolicy::PickerOnly`] → always [`AttachDecision::Stay`].
/// 2. Resolve `root = project_root(new_cwd)`.
/// 3. If `current` is `Some` and its `project_root == root` →
///    [`AttachDecision::Stay`] (cd *within* a project never switches).
/// 4. Else if `binding.lookup(root)` is `Some(id)` AND `index` still has
///    that session → [`AttachDecision::SwitchTo(id)`].
/// 5. Else → [`AttachDecision::SpawnNew`] with the deterministic
///    [`FleetSessionNames::from_project_path`] name for `root`.
///
/// Under [`AttachPolicy::SuggestOnly`] the same Switch/Spawn is computed
/// then wrapped in [`AttachDecision::Suggest`]; a `Stay` is never
/// wrapped (there is nothing to suggest).
///
/// Pure — uses [`project_root`] (real-fs marker probe) only to classify
/// `new_cwd`; everything else is in-memory. For fully-hermetic tests use
/// [`decide_with_root`] which takes a pre-resolved root.
#[must_use]
pub fn decide(
    policy: AttachPolicy,
    current: Option<&SessionRecord>,
    new_cwd: &Path,
    binding: &ProjectBinding,
    index: &SessionIndex,
    style: SessionNameStyle,
) -> AttachDecision {
    let root = project_root(new_cwd);
    decide_with_root(policy, current, &root, binding, index, style)
}

/// [`decide`] with the project root already resolved — pure +
/// filesystem-free, so the branch logic is testable without a real
/// directory tree.
#[must_use]
pub fn decide_with_root(
    policy: AttachPolicy,
    current: Option<&SessionRecord>,
    root: &Path,
    binding: &ProjectBinding,
    index: &SessionIndex,
    style: SessionNameStyle,
) -> AttachDecision {
    if policy == AttachPolicy::PickerOnly {
        return AttachDecision::Stay;
    }

    // cd within the same project never switches.
    if let Some(cur) = current {
        if cur.project_root == root {
            return AttachDecision::Stay;
        }
    }

    let raw = match binding.lookup(root) {
        Some(id) if index.get(id).is_some() => AttachDecision::SwitchTo(id),
        _ => AttachDecision::SpawnNew {
            project_root: root.to_path_buf(),
            name: FleetSessionNames::from_project_path(root, style),
        },
    };

    match policy {
        AttachPolicy::AutoSwitch => raw,
        AttachPolicy::SuggestOnly => AttachDecision::Suggest(Box::new(raw)),
        AttachPolicy::PickerOnly => AttachDecision::Stay, // unreachable; handled above
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn sid(s: &str) -> SessionId {
        SessionId::from_seed(s)
    }

    fn record_for(id: &str, root: &str) -> SessionRecord {
        SessionRecord::for_project(sid(id), PathBuf::from(root), SessionNameStyle::Emoji, 0)
    }

    #[test]
    fn same_project_stays() {
        let cur = record_for("cur", "/code/mado");
        let binding = ProjectBinding::new();
        let index = SessionIndex::new();
        let d = decide_with_root(
            AttachPolicy::AutoSwitch,
            Some(&cur),
            Path::new("/code/mado"),
            &binding,
            &index,
            SessionNameStyle::Emoji,
        );
        assert_eq!(d, AttachDecision::Stay);
    }

    #[test]
    fn bound_other_project_switches() {
        let cur = record_for("cur", "/code/mado");
        let mut binding = ProjectBinding::new();
        binding.bind(PathBuf::from("/code/tear"), sid("tear-sess"));
        let mut index = SessionIndex::new();
        index.upsert(record_for("tear-sess", "/code/tear"));
        let d = decide_with_root(
            AttachPolicy::AutoSwitch,
            Some(&cur),
            Path::new("/code/tear"),
            &binding,
            &index,
            SessionNameStyle::Emoji,
        );
        assert_eq!(d, AttachDecision::SwitchTo(sid("tear-sess")));
    }

    #[test]
    fn bound_but_session_gone_spawns_new() {
        // Binding points at a dead session (not in the index) -> spawn.
        let mut binding = ProjectBinding::new();
        binding.bind(PathBuf::from("/code/tear"), sid("dead"));
        let index = SessionIndex::new(); // empty — session gone
        let d = decide_with_root(
            AttachPolicy::AutoSwitch,
            None,
            Path::new("/code/tear"),
            &binding,
            &index,
            SessionNameStyle::Emoji,
        );
        match d {
            AttachDecision::SpawnNew { project_root, name } => {
                assert_eq!(project_root, PathBuf::from("/code/tear"));
                let expected =
                    FleetSessionNames::from_project_path(Path::new("/code/tear"), SessionNameStyle::Emoji);
                assert_eq!(name.to_string(), expected.to_string());
            }
            other => panic!("expected SpawnNew, got {other:?}"),
        }
    }

    #[test]
    fn brand_new_project_spawns_with_deterministic_name() {
        let binding = ProjectBinding::new();
        let index = SessionIndex::new();
        let d = decide_with_root(
            AttachPolicy::AutoSwitch,
            None,
            Path::new("/code/pleme-io/brand-new"),
            &binding,
            &index,
            SessionNameStyle::Emoji,
        );
        match d {
            AttachDecision::SpawnNew { project_root, name } => {
                assert_eq!(project_root, PathBuf::from("/code/pleme-io/brand-new"));
                let expected = FleetSessionNames::from_project_path(
                    Path::new("/code/pleme-io/brand-new"),
                    SessionNameStyle::Emoji,
                );
                assert_eq!(name.to_string(), expected.to_string());
                // determinism: same path -> same name again.
                let d2 = decide_with_root(
                    AttachPolicy::AutoSwitch,
                    None,
                    Path::new("/code/pleme-io/brand-new"),
                    &binding,
                    &index,
                    SessionNameStyle::Emoji,
                );
                assert_eq!(AttachDecision::SpawnNew { project_root, name }, d2);
            }
            other => panic!("expected SpawnNew, got {other:?}"),
        }
    }

    #[test]
    fn picker_only_always_stays() {
        // Even with a perfectly good binding, PickerOnly never acts.
        let mut binding = ProjectBinding::new();
        binding.bind(PathBuf::from("/code/tear"), sid("tear-sess"));
        let mut index = SessionIndex::new();
        index.upsert(record_for("tear-sess", "/code/tear"));
        let d = decide_with_root(
            AttachPolicy::PickerOnly,
            None,
            Path::new("/code/tear"),
            &binding,
            &index,
            SessionNameStyle::Emoji,
        );
        assert_eq!(d, AttachDecision::Stay);
    }

    #[test]
    fn suggest_only_wraps_switch() {
        let mut binding = ProjectBinding::new();
        binding.bind(PathBuf::from("/code/tear"), sid("tear-sess"));
        let mut index = SessionIndex::new();
        index.upsert(record_for("tear-sess", "/code/tear"));
        let d = decide_with_root(
            AttachPolicy::SuggestOnly,
            None,
            Path::new("/code/tear"),
            &binding,
            &index,
            SessionNameStyle::Emoji,
        );
        assert_eq!(
            d,
            AttachDecision::Suggest(Box::new(AttachDecision::SwitchTo(sid("tear-sess"))))
        );
    }

    #[test]
    fn suggest_only_does_not_wrap_stay() {
        // cd within the same project -> Stay, never Suggest(Stay).
        let cur = record_for("cur", "/code/mado");
        let binding = ProjectBinding::new();
        let index = SessionIndex::new();
        let d = decide_with_root(
            AttachPolicy::SuggestOnly,
            Some(&cur),
            Path::new("/code/mado"),
            &binding,
            &index,
            SessionNameStyle::Emoji,
        );
        assert_eq!(d, AttachDecision::Stay);
    }
}
