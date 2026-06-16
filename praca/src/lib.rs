//! `praca` — the praça session-orchestration substrate for the
//! mado/tear terminal.
//!
//! **Automation-first.** Sessions are auto-named (deterministic glyph +
//! word from the project path, via `ishou_tokens::FleetSessionNames`)
//! and auto-bound to projects; `cd` auto-attaches the project's session;
//! the picker is the *fallback* for everything automation doesn't cover.
//!
//! A *praça* (Portuguese: a town square) is where the sessions of a
//! workspace gather — every project root has its named seat, and walking
//! into a project (cd) seats you at it automatically.
//!
//! ## The pieces
//!
//! * [`project`] — walk UP from a cwd to the nearest project root
//!   (`.git`, else `Cargo.toml`/`flake.nix`/`package.json`/`go.mod`/
//!   `pyproject.toml`). Pure, mock-testable.
//! * [`record`] — [`SessionRecord`]: the persisted, ranked, searchable
//!   unit per session (id, name seed+style, project root, cwd, frecency
//!   counters, tags, state).
//! * [`frecency`] — recency-weighted frequency [`frecency::score`].
//! * [`binding`] — [`ProjectBinding`]: the persisted project→session
//!   map that powers cd-driven auto-attach.
//! * [`index`] — [`SessionIndex`]: fuzzy + frecency search over every
//!   session (the picker), plus by-project lookup.
//! * [`attach`] — the automation core: [`attach::decide`] turns a cd
//!   into a typed [`AttachDecision`] under an [`AttachPolicy`].
//!
//! ## This phase is pure
//!
//! No mado/tear-daemon wiring lives here yet — that is a later phase.
//! Every function is deterministic: time is injected as `u64`
//! unix-seconds, never read from the clock; the filesystem is touched
//! only by [`project::project_root`]'s real-fs marker probe (and even
//! that has a pure [`project::project_root_with`] sibling for tests).
//! The [`Praca`] facade is the surface mado/tear will drive later.

#![forbid(unsafe_code)]

pub mod attach;
pub mod binding;
pub mod frecency;
pub mod index;
pub mod project;
pub mod record;

pub use attach::{decide, decide_with_root, AttachDecision, AttachPolicy};
pub use binding::ProjectBinding;
pub use index::SessionIndex;
pub use project::{find_project_root, find_project_root_with, project_root, project_root_with};
pub use record::{NameStyle, SessionRecord, SessionState, ThemeMirror};
pub use snapshot::{PolicyMirror, PracaSnapshot};

// Re-export the ishou name primitives so consumers (tear-daemon, mado) can
// build a `SessionRecord` without depending on ishou_tokens directly —
// praça is the single surface the daemon/UI speak to.
pub use ishou_tokens::{SessionNameStyle, SessionTheme};

pub mod snapshot;

use std::path::Path;

use tear_types::id::SessionId;

/// The session-orchestration facade — the surface mado/tear drives.
///
/// Holds the live [`SessionIndex`], the persisted [`ProjectBinding`]
/// map, the active [`AttachPolicy`], and the [`SessionNameStyle`] used
/// to auto-name new sessions. All mutation flows through typed methods;
/// time is always injected.
#[derive(Clone, Debug)]
pub struct Praca {
    /// Searchable catalog of every tracked session.
    pub index: SessionIndex,
    /// Persisted project→session bindings (the cd-attach memory).
    pub binding: ProjectBinding,
    /// How aggressively `cd` auto-attaches.
    pub policy: AttachPolicy,
    /// Style for auto-generated session names.
    pub name_style: SessionNameStyle,
}

impl Default for Praca {
    fn default() -> Self {
        Self::new()
    }
}

impl Praca {
    /// A fresh, empty orchestrator with the automation-first defaults
    /// ([`AttachPolicy::AutoSwitch`], emoji session names).
    #[must_use]
    pub fn new() -> Self {
        Self {
            index: SessionIndex::new(),
            binding: ProjectBinding::new(),
            policy: AttachPolicy::AutoSwitch,
            name_style: SessionNameStyle::default(),
        }
    }

    /// Build with an explicit policy + name style (e.g. restored from
    /// config).
    #[must_use]
    pub fn with(
        index: SessionIndex,
        binding: ProjectBinding,
        policy: AttachPolicy,
        name_style: SessionNameStyle,
    ) -> Self {
        Self { index, binding, policy, name_style }
    }

    /// Decide what to do when the operator `cd`s to `new_cwd`. The
    /// `current_id` is the session the operator is presently attached to
    /// (if any); `now` is unix-seconds (injected — unused by the
    /// decision itself but kept on the signature so the facade is the
    /// single time-injection point as later phases record visits inline).
    ///
    /// Returns a typed [`AttachDecision`] for the caller to perform. This
    /// method does NOT mutate — performing a switch/spawn and recording
    /// the resulting visit is the caller's job (it knows the real
    /// SessionId the daemon minted for a spawn).
    #[must_use]
    pub fn on_cwd_change(
        &self,
        current_id: Option<SessionId>,
        new_cwd: &Path,
        now: u64,
    ) -> AttachDecision {
        let _ = now;
        let current = current_id.and_then(|id| self.index.get(id));
        attach::decide(
            self.policy,
            current,
            new_cwd,
            &self.binding,
            &self.index,
            self.name_style,
        )
    }

    /// Record a visit to a tracked session: bump its visit count and set
    /// `last_seen` to the injected `now`. No-op if the session is not in
    /// the index. Returns whether a record was updated.
    pub fn record_visit(&mut self, id: SessionId, now: u64) -> bool {
        if let Some(r) = self.index.get_mut(id) {
            r.visits = r.visits.saturating_add(1);
            r.last_seen = now;
            true
        } else {
            false
        }
    }

    /// Search the session index (the picker surface). Empty query →
    /// every session by frecency; non-empty → fuzzy-filtered + ranked.
    /// `now` is injected for the frecency tie-break.
    #[must_use]
    pub fn search(&self, query: &str, now: u64) -> Vec<&SessionRecord> {
        self.index.search(query, now)
    }

    /// Capture the persistable state as a serde-friendly
    /// [`PracaSnapshot`] — the index records, the binding map, the
    /// attach policy, and the name style. This is what the daemon
    /// writes to disk so bindings + frecency survive a restart.
    #[must_use]
    pub fn to_snapshot(&self) -> PracaSnapshot {
        PracaSnapshot {
            index: self.index.clone(),
            binding: self.binding.clone(),
            policy: self.policy.into(),
            name_style: self.name_style.into(),
        }
    }

    /// Rebuild a [`Praca`] facade from a persisted [`PracaSnapshot`] —
    /// the inverse of [`Self::to_snapshot`]. Used by the daemon at
    /// startup to reload the bindings + frecency it persisted in a
    /// prior run.
    #[must_use]
    pub fn from_snapshot(snap: PracaSnapshot) -> Self {
        Self {
            index: snap.index,
            binding: snap.binding,
            policy: snap.policy.into(),
            name_style: snap.name_style.into(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn sid(s: &str) -> SessionId {
        SessionId::from_seed(s)
    }

    #[test]
    fn facade_defaults_are_automation_first() {
        let p = Praca::new();
        assert_eq!(p.policy, AttachPolicy::AutoSwitch);
        assert_eq!(p.name_style, SessionNameStyle::Emoji);
        assert!(p.index.is_empty());
    }

    #[test]
    fn on_cwd_change_spawns_for_new_project() {
        let p = Praca::new();
        let d = p.on_cwd_change(None, Path::new("/code/pleme-io/brand-new"), 100);
        assert!(matches!(d, AttachDecision::SpawnNew { .. }));
    }

    #[test]
    fn on_cwd_change_switches_to_bound_session() {
        let mut p = Praca::new();
        let rec =
            SessionRecord::for_project(sid("tear-sess"), PathBuf::from("/code/tear"), SessionNameStyle::Emoji, 0);
        p.index.upsert(rec);
        p.binding.bind(PathBuf::from("/code/tear"), sid("tear-sess"));
        let d = p.on_cwd_change(None, Path::new("/code/tear"), 100);
        assert_eq!(d, AttachDecision::SwitchTo(sid("tear-sess")));
    }

    #[test]
    fn record_visit_bumps_and_stamps() {
        let mut p = Praca::new();
        let rec = SessionRecord::for_project(sid("s"), PathBuf::from("/x"), SessionNameStyle::Emoji, 10);
        p.index.upsert(rec);
        let before = p.index.get(sid("s")).unwrap().visits;
        assert!(p.record_visit(sid("s"), 999));
        let after = p.index.get(sid("s")).unwrap();
        assert_eq!(after.visits, before + 1);
        assert_eq!(after.last_seen, 999);
        // missing session -> no-op false.
        assert!(!p.record_visit(sid("nope"), 999));
    }

    #[test]
    fn search_delegates_to_index() {
        let mut p = Praca::new();
        p.index.upsert(SessionRecord::for_project(
            sid("a"),
            PathBuf::from("/code/mado"),
            SessionNameStyle::Emoji,
            100,
        ));
        let out = p.search("mado", 100);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].id, sid("a"));
    }
}
