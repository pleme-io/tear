//! The live half of the session model — a *running* incarnation and its
//! durability.
//!
//! A [`LiveSession`] is what the daemon owns: real PTYs, a mutable
//! layout, scrollback. It is reached by *instantiating* a definition
//! (see praça's `SessionDefinition` + the `instantiate` morphism), and a
//! daemon restart does NOT resurrect it — it re-instantiates the
//! definition under a *fresh* [`InstanceId`]. The [`Durability`] marker
//! types how far a live session's processes reach: `ProcessBound` dies
//! with the daemon; `Held` (a tamotsu holder owns the PTY outside the
//! daemon) survives a daemon restart and is re-adopted. Nothing names
//! survival of a reboot or logout — after one, a held session is
//! *resurrected* as a new incarnation from its makimono journal
//! (docs/SESSION-DURABILITY.md), so "these processes survived a reboot"
//! stays **unrepresentable** (pressure-test illegal state #6, narrowed).

use serde::{Deserialize, Serialize};

use crate::{
    id::{DefinitionId, InstanceId},
    session::TearSession,
};

/// Durability of a live session's runtime state. `ProcessBound`: the
/// PTYs live in the daemon and die with it. `Held`: a holder process
/// outside the daemon owns them, so a daemon restart re-adopts the same
/// processes. No arm claims survival of a reboot — that is a world fact
/// (every process of the user dies), answered by resurrection, which is a
/// fresh incarnation and never a claimed survivor.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Durability {
    /// State lives in the daemon process and is lost when it exits.
    /// Recovery is re-instantiation of the definition, never resurrection.
    ProcessBound,
    /// The PTY and child are held by a tamotsu holder outside the daemon;
    /// a daemon restart re-adopts them. A reboot is not covered.
    Held,
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
            freio: crate::freio::Freio::Released,
        }
    }

    #[test]
    fn durability_names_daemon_restart_survival_and_nothing_beyond_it() {
        // The forcing function: an arm claiming reboot survival would fail
        // this exhaustive match and has to be argued for here.
        for d in [Durability::ProcessBound, Durability::Held] {
            match d {
                Durability::ProcessBound | Durability::Held => {}
            }
        }
        assert_eq!(Durability::default(), Durability::ProcessBound);
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
