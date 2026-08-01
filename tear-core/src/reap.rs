//! The typed admission ticket for **daemon-initiated** session removal.
//!
//! # Why this type exists
//!
//! A multiplexer's whole reason to exist is that a session outlives every
//! client: you detach, the shell keeps running, you re-attach. Any rule of
//! the form *"end this session because nobody is currently attached"*
//! contradicts the product, and it is the kind of rule that reads as
//! housekeeping when written and is only discovered as data loss when an
//! operator loses a session.
//!
//! It was written. `tear-daemon`'s "orphan pruner" (deleted 2026-07-31)
//! ticked every 2 s and killed any session with zero live byte-stream
//! subscribers — 3 s of grace for `SessionSource::Named`, 15 s for
//! `SessionSource::Agent`, never for `Human`. Measured on a scratch daemon
//! at 01:10:54 with three sessions whose shells were all *running*:
//!
//! ```text
//! t=+0s   S-human  S-named  S-agent
//! t=+4s   S-human           S-agent     ← named killed after 3 s
//! t=+16s  S-human                       ← agent killed after 15 s
//! ```
//!
//! Nothing in the call path errored — that is what made it so expensive to
//! find. Every RPC returned `Ok`, `get_session` succeeded, `send_keys`
//! landed; the session was simply gone a moment later. Two consumers were
//! bitten at once: banken's bancada (`Named`, so it died ~3 s after being
//! opened, before the operator could attach) and mado's `tear_new_session`
//! MCP tool (`Agent`, ~15 s).
//!
//! # The seal
//!
//! [`AllPanesExited`] is the only value that admits a session to
//! [`crate::InProcess::reap_proven_dead_session`], and its sole constructor
//! walks a [`TearSession`]'s [`PaneState`]s. There is deliberately **no**
//! constructor that reads a subscriber count, a client connection, an idle
//! timer, or a [`tear_types::SessionSource`] — so the deleted policy cannot
//! be re-expressed against this API. Reconstructing it would mean adding a
//! constructor here, in a file that says why not to.
//!
//! **Tier, stated honestly: parse-time-rejected at the daemon boundary, not
//! truly-unrepresentable.** The private field makes `AllPanesExited`
//! unconstructible outside `tear-core`, so a background thread in
//! `tear-daemon` genuinely cannot compile a subscriber-keyed reap through
//! this door. It is *not* a total ban on ending a session: the
//! operator-facing `MultiplexerControl::kill_session` is still reachable
//! from any code holding an `InProcess`, by construction — it is how `tear
//! kill` works. The seal binds the daemon's own initiative, which is where
//! the defect lived.
//!
//! Provenance ([`tear_types::SessionSource`]) is audit metadata for
//! `tear list --source`. It is not a lifetime, and nothing here reads it.

use tear_types::{PaneState, SessionId, TearSession};

/// Proof that a session is dead: it has at least one pane, and **every**
/// pane's child process has exited.
///
/// See the [module docs](self) for why the constructor is this narrow.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AllPanesExited {
    session: SessionId,
}

impl AllPanesExited {
    /// Witness the session's death, or `None` if it is still alive.
    ///
    /// `None` for a session with **no panes at all**: an empty pane map
    /// makes `all(…)` vacuously true, and a vacuous proof is exactly the
    /// kind of guard that examines nothing. A pane-less session is a
    /// normal state in tear — `kill_last_pane_closes_the_window` asserts
    /// that killing a window's only pane leaves the session gettable with
    /// no windows and no panes — never evidence that a shell died.
    ///
    /// This is a deliberate (tiny) behaviour change: the pre-2026-07-31
    /// inline re-verify used a bare `all(…)`, so a `kill_pane` that
    /// emptied the session in the window between marking a pane exited
    /// and re-checking would silently delete the session record. Every
    /// other path leaves such a husk in place; the race path now agrees
    /// with them.
    #[must_use]
    pub fn witness(session: &TearSession) -> Option<Self> {
        if session.panes.is_empty() {
            return None;
        }
        session
            .panes
            .values()
            .all(|p| matches!(p.state, PaneState::Exited { .. }))
            .then_some(Self {
                session: session.id,
            })
    }

    /// The session this proof is about. A proof is not transferable to a
    /// different id — the reaper reads the target from here rather than
    /// taking it as a second, unrelated argument.
    #[must_use]
    pub const fn session(self) -> SessionId {
        self.session
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use tear_types::{
        InputPolicy, LayoutNode, PaneId, SessionSource, SessionState, TearPane, TearWindow,
        WindowId, WindowState,
    };

    use super::*;

    fn pane(id: u64, state: PaneState) -> TearPane {
        TearPane {
            // A test fixture has no spawning connection, and `Unknown` is
            // the honest answer for one.
            yurai: tear_types::Yurai::Unknown,
            id: PaneId(id),
            shell: "/bin/sh".into(),
            args: vec![],
            cwd: None,
            env: vec![],
            size_cells: (80, 24),
            origin_cells: (0, 0),
            state,
            title: "/bin/sh".into(),
            input_policy: InputPolicy::default(),
        }
    }

    fn session(source: SessionSource, panes: Vec<TearPane>) -> TearSession {
        let wid = WindowId(1);
        let mut pane_map = BTreeMap::new();
        for p in panes {
            pane_map.insert(p.id, p);
        }
        let mut windows = BTreeMap::new();
        windows.insert(
            wid,
            TearWindow {
                id: wid,
                name: "main".into(),
                layout: LayoutNode::leaf(PaneId(1)),
                active_pane: PaneId(1),
                size_cells: (80, 24),
                state: WindowState::Active,
            },
        );
        TearSession {
            id: SessionId(7),
            name: "s".into(),
            windows,
            panes: pane_map,
            active_window: wid,
            state: SessionState::Active,
            created_at_unix: 0,
            description: String::new(),
            source,
        }
    }

    #[test]
    fn every_pane_exited_witnesses_the_session() {
        let s = session(
            SessionSource::Human,
            vec![
                pane(1, PaneState::Exited { code: 0 }),
                pane(2, PaneState::Exited { code: 130 }),
            ],
        );
        assert_eq!(AllPanesExited::witness(&s).map(AllPanesExited::session), Some(s.id));
    }

    /// The defect, as a property. A session with ANY live pane is
    /// un-witnessable — and because the witness is the only admission
    /// ticket, no daemon-side rule can end it on its own initiative.
    #[test]
    fn one_live_pane_refuses_the_witness_whatever_the_provenance() {
        for source in [
            SessionSource::Human,
            SessionSource::Agent,
            SessionSource::Named("banken-bancada".into()),
        ] {
            for live in [PaneState::Running, PaneState::Spawning] {
                let s = session(
                    source.clone(),
                    vec![pane(1, PaneState::Exited { code: 0 }), pane(2, live)],
                );
                assert!(
                    AllPanesExited::witness(&s).is_none(),
                    "a {source:?} session with a {live:?} pane must not be reapable"
                );
            }
        }
    }

    #[test]
    fn all_running_refuses_the_witness() {
        let s = session(
            SessionSource::Named("banken-bancada".into()),
            vec![pane(1, PaneState::Running), pane(2, PaneState::Running)],
        );
        assert!(AllPanesExited::witness(&s).is_none());
    }

    /// A vacuous proof is not a proof — see [`AllPanesExited::witness`].
    #[test]
    fn a_pane_less_session_is_not_witnessed() {
        let s = session(SessionSource::Human, vec![]);
        assert!(AllPanesExited::witness(&s).is_none());
    }
}
