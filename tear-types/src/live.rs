//! The live half of the session model — a *running* incarnation and its
//! durability.
//!
//! A [`LiveSession`] is what the daemon owns: real PTYs, a mutable
//! layout, scrollback. It is reached by *instantiating* a definition
//! (see praça's `SessionDefinition` + the `instantiate` morphism), and a
//! daemon restart does NOT resurrect it — it re-instantiates the
//! definition under a *fresh* [`InstanceId`]. The [`Durability`] marker
//! is how that impossibility is made typed: there is no value of
//! `Durability` that means "survives a restart", so a live session
//! claiming restart-durability is **unrepresentable** (pressure-test
//! illegal state #6). The only thing that survives a restart is the
//! definition (durable, in praça's store); the live processes are, by
//! type, process-bound.

use serde::{Deserialize, Serialize};

use crate::{
    id::{DefinitionId, InstanceId},
    session::TearSession,
};

/// Durability of a live session's runtime state. The marker exists to
/// make one illegal claim unrepresentable: a [`LiveSession`]'s PTYs,
/// layout, and scrollback live in the daemon process and die with it, so
/// there is no `Durable` / `SurvivesRestart` arm to construct. "Restart
/// the session" is therefore not "resurrect these processes" (no value
/// expresses that) but "re-instantiate the definition" — a fresh
/// incarnation. New arms would only ever describe *finer* process-bound
/// lifetimes, never a restart-surviving one.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Durability {
    /// State lives in the daemon process and is lost when it exits.
    /// Recovery is re-instantiation of the definition, never resurrection.
    ProcessBound,
}

impl Default for Durability {
    fn default() -> Self {
        Self::ProcessBound
    }
}

/// A running session incarnation: the shipped [`TearSession`] runtime
/// state plus the typed link back to the [`DefinitionId`] it was
/// instantiated from, plus its [`Durability`] marker.
///
/// `LiveSession` is a *graceful extension* of [`TearSession`] — it embeds
/// it as-is rather than re-modelling windows/panes — and adds exactly the
/// two facts the pressure-test found missing: which definition this live
/// session realizes (illegal state #1/#5 — the typed live→definition
/// link, so a stale handle isn't conflated with a durable identity), and
/// that it is process-bound (illegal state #6).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct LiveSession {
    /// The definition this incarnation was instantiated from. A restart
    /// re-instantiates THIS definition under a new [`InstanceId`]; the
    /// link is how the daemon knows which definition a live session
    /// realizes (and how 1 definition → N live instances is tracked, in
    /// praça's `InstanceRegistry`).
    pub definition: DefinitionId,
    /// Durability marker — always [`Durability::ProcessBound`]; there is
    /// no restart-surviving value to set it to.
    #[serde(default)]
    pub durability: Durability,
    /// The runtime session state (windows, panes, live layout,
    /// scrollback-bearing pane ids). Embedded as-is.
    pub session: TearSession,
}

impl LiveSession {
    /// Construct a live session from a freshly-spawned [`TearSession`] and
    /// the definition it realizes. Always [`Durability::ProcessBound`].
    #[must_use]
    pub fn new(definition: DefinitionId, session: TearSession) -> Self {
        Self {
            definition,
            durability: Durability::ProcessBound,
            session,
        }
    }

    /// This incarnation's spawn-unique handle — the embedded session's id,
    /// not a stored duplicate (no drift between two id fields). Typed as
    /// [`InstanceId`] to document that it is the LIVE handle, distinct
    /// from `self.definition`.
    #[must_use]
    pub fn instance(&self) -> InstanceId {
        self.session.id
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::id::{DefinitionId, SessionId, WindowId};
    use crate::session::{SessionSource, SessionState};
    use std::collections::BTreeMap;
    use std::path::Path;

    fn sample_session() -> TearSession {
        // Compile-checked literal — if TearSession gains/changes a field
        // this test breaks at COMPILE time (a forcing function), not at
        // runtime parse.
        TearSession {
            id: SessionId(1234),
            name: "demo".into(),
            windows: BTreeMap::new(),
            panes: BTreeMap::new(),
            active_window: WindowId::NULL,
            state: SessionState::Active,
            created_at_unix: 0,
            description: String::new(),
            source: SessionSource::Human,
        }
    }

    #[test]
    fn durability_has_no_restart_surviving_value() {
        // The whole guarantee in one line: the only constructible value is
        // ProcessBound. (If a `Durable` arm were ever added, this match
        // would fail to compile — the forcing function.)
        let d = Durability::default();
        match d {
            Durability::ProcessBound => {}
        }
    }

    #[test]
    fn live_session_links_to_its_definition_and_is_process_bound() {
        let def = DefinitionId::from_project(Path::new("/code/pleme-io/mado"));
        let live = LiveSession::new(def, sample_session());
        assert_eq!(live.definition, def);
        assert_eq!(live.durability, Durability::ProcessBound);
        // instance() reads through to the embedded session — one id, no drift.
        assert_eq!(live.instance(), live.session.id);
    }

    #[test]
    fn live_session_serde_round_trips() {
        let def = DefinitionId::from_project(Path::new("/x"));
        let live = LiveSession::new(def, sample_session());
        let json = serde_json::to_string(&live).unwrap();
        let back: LiveSession = serde_json::from_str(&json).unwrap();
        assert_eq!(live, back);
    }
}
