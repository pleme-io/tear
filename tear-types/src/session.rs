//! Session — a top-level grouping of windows that survives across
//! client disconnects.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::{
    freio::{Admission, Freio, RefusalReason},
    id::{PaneId, SessionId, WindowId},
    pane::{InputPolicy, TearPane},
    window::TearWindow,
    yurai::Yurai,
};

/// One session: the top-level entity in the multiplexer hierarchy.
/// A session owns a set of windows; each window owns a layout tree
/// of panes. Sessions persist across client attach/detach cycles —
/// this is what makes tear (and tmux) a *multiplexer* rather than a
/// shell wrapper.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct TearSession {
    pub id: SessionId,
    /// Operator-visible session name (`"work"`, `"infra"`,
    /// `"deploy-staging"`). Stable across renames? No — `tear rename`
    /// mutates without minting a new ID.
    pub name: String,
    /// Windows belonging to this session, keyed by id. BTreeMap so
    /// the wire format orders deterministically.
    pub windows: BTreeMap<WindowId, TearWindow>,
    /// Panes belonging to this session, keyed by id. Stored flat at
    /// the session level so a pane can move between windows without
    /// changing its address (tmux's `join-pane` semantics).
    pub panes: BTreeMap<PaneId, TearPane>,
    /// Currently-focused window id. Must exist in `windows`.
    pub active_window: WindowId,
    /// Lifecycle state.
    pub state: SessionState,
    /// Unix-seconds-since-epoch when this session was created.
    pub created_at_unix: u64,
    /// Optional operator-set description / notes — surfaced by
    /// `tear list` and by the status bar.
    #[serde(default)]
    pub description: String,
    /// Provenance — who/what created this session. Lets operators
    /// audit at a glance whether a session was opened by a human
    /// shell, by an AI agent (via mado MCP / direct UDS), or by a
    /// named automation. `tear list --source agent` filters; mado
    /// MCP tools default to `Source::Agent` so an operator's
    /// `tear list` separates "what I started" from "what the agent
    /// started behind my back". Default = `Source::Human` (the
    /// safe assumption when nothing said otherwise — pre-#6
    /// sessions deserialise as Human).
    #[serde(default)]
    pub source: SessionSource,
    /// The operator's brake. `#[serde(default)]` → [`Freio::Released`],
    /// which is what every pre-freio session record already means, so
    /// landing this field changes no existing behaviour.
    #[serde(default)]
    pub freio: Freio,
}

impl TearSession {
    /// What input a pane ACTUALLY accepts right now.
    ///
    /// **The only way to answer this question.** Note what deliberately
    /// does not exist: a `TearPane::admits()`. A pane alone cannot answer
    /// it — the brake lives on the session — and a method that pretended
    /// otherwise is exactly how a pane comes to report `Free` while
    /// refusing input. The absent method is the seal.
    ///
    /// This also JOINS two gates that already exist rather than adding a
    /// third: the `Locked` check inside `tear-core::send_keys` and the
    /// `Leader` check in the daemon's serve loop. Two authorities over one
    /// question was already one too many; freio must not make it three.
    ///
    /// ## The ordering is the design
    ///
    /// The brake is consulted BEFORE the policy lattice. That is what
    /// makes it non-advisory: a pane explicitly pinned to `Free` still
    /// cannot escape a brake, because the brake is answered before the pin
    /// is ever read.
    #[must_use]
    pub fn admits(&self, pane: PaneId) -> Option<Admission> {
        let p = self.panes.get(&pane)?;
        // Brake first — see above.
        //
        // Only `Automation` panes are braked. `Unknown` and `Human` panes
        // keep accepting input, deliberately: a brake that can lock the
        // operator out of their own terminal during the emergency they
        // engaged it for is worse than no brake. The cost is that a pane
        // the daemon could not classify SURVIVES the brake — an honest
        // miss, which the CLI reports by name rather than hiding.
        if self.freio.is_engaged() && p.yurai.is_automation() {
            return Some(Admission::Refuse(RefusalReason::Freio));
        }
        Some(match p.input_policy {
            InputPolicy::Free => Admission::Accept,
            InputPolicy::Locked => Admission::Refuse(RefusalReason::Policy),
            InputPolicy::Leader { id } => Admission::OnlyLeader { id },
        })
    }

    /// Panes this session's brake would NOT stop, because their
    /// provenance is unknown.
    ///
    /// Exists so the miss above is reportable. An operator pressing a
    /// panic button must be told what it did not reach — silence here
    /// would let them believe everything stopped.
    #[must_use]
    pub fn unbrakable(&self) -> Vec<PaneId> {
        self.panes
            .iter()
            .filter(|(_, p)| matches!(p.yurai, Yurai::Unknown))
            .map(|(id, _)| *id)
            .collect()
    }
}

/// Session lifecycle states.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SessionState {
    /// Session has at least one window with at least one running pane.
    Active,
    /// All windows closed; session retained per
    /// `destroy-unattached off` semantics until explicitly killed.
    Detached,
}

/// Who/what created a session. Operator-visible provenance.
/// Internally tagged so the wire shape stays compact + future
/// `Named(_)` etc. can land without churning the variant ordering.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "kind", content = "id", rename_all = "snake_case")]
pub enum SessionSource {
    /// A human user (CLI, `tear up`, ghostty/iTerm interactive shell).
    Human,
    /// An AI agent — Claude Code, Cursor, OpenCode, the mado MCP
    /// surface. Default for sessions created via the MCP path so
    /// operators can `tear list --source agent` and triage.
    Agent,
    /// A named automation (CI job, scheduled task, sidecar). The
    /// id is operator-defined — e.g. `"pleme-ci-deploy"` —
    /// surfaced verbatim in `tear list`. Lets one daemon hold
    /// sessions from many automations without colliding under a
    /// single `Agent` bucket.
    Named(String),
}

impl Default for SessionSource {
    fn default() -> Self {
        SessionSource::Human
    }
}

impl SessionSource {
    /// Short label for `tear list` text output.
    #[must_use]
    pub fn label(&self) -> &str {
        match self {
            SessionSource::Human => "human",
            SessionSource::Agent => "agent",
            SessionSource::Named(_) => "named",
        }
    }
}

#[cfg(test)]
mod freio_rows {
    use super::*;
    use crate::pane::PaneState;
    use std::collections::BTreeMap;

    fn pane(id: u64, yurai: Yurai, input_policy: InputPolicy) -> TearPane {
        TearPane {
            id: PaneId(id),
            shell: "/bin/sh".into(),
            args: vec![],
            cwd: None,
            env: vec![],
            size_cells: (80, 24),
            origin_cells: (0, 0),
            state: PaneState::Running,
            title: "sh".into(),
            input_policy,
            yurai,
        }
    }

    fn session(panes: Vec<TearPane>, freio: Freio) -> TearSession {
        let mut m = BTreeMap::new();
        for p in panes {
            m.insert(p.id, p);
        }
        TearSession {
            id: SessionId(1),
            name: "s".into(),
            windows: BTreeMap::new(),
            panes: m,
            active_window: WindowId(1),
            state: SessionState::Active,
            created_at_unix: 0,
            description: String::new(),
            source: SessionSource::Human,
            freio,
        }
    }

    const ON: Freio = Freio::Engaged { at_unix: 1 };

    /// The brake stops automation and leaves everything else alone.
    #[test]
    fn freio_brakes_only_automation_panes() {
        let s = session(
            vec![
                pane(1, Yurai::Automation { label: None }, InputPolicy::Free),
                pane(2, Yurai::Human, InputPolicy::Free),
                pane(3, Yurai::Unknown, InputPolicy::Free),
            ],
            ON,
        );
        assert_eq!(
            s.admits(PaneId(1)),
            Some(Admission::Refuse(RefusalReason::Freio))
        );
        assert_eq!(s.admits(PaneId(2)), Some(Admission::Accept));
        assert_eq!(
            s.admits(PaneId(3)),
            Some(Admission::Accept),
            "an UNKNOWN pane must survive the brake — operator decision \
             2026-08-01. A panic button that can lock you out of your own \
             terminal during the emergency you pressed it for is not one."
        );
    }

    /// ★ THE ORDERING ROW. The brake is consulted BEFORE the policy
    /// lattice, which is the whole reason it is not advisory: a pane
    /// explicitly pinned to `Free` still cannot escape it.
    #[test]
    fn an_explicitly_free_automation_pane_cannot_escape_the_brake() {
        let s = session(
            vec![pane(
                1,
                Yurai::Automation { label: None },
                InputPolicy::Free,
            )],
            ON,
        );
        assert_eq!(
            s.admits(PaneId(1)),
            Some(Admission::Refuse(RefusalReason::Freio)),
            "checking the policy first would let an explicitly-Free pane \
             walk straight through the brake"
        );
    }

    /// A braked automation pane refuses for the RIGHT reason — the two
    /// refusals send an operator to different fixes.
    #[test]
    fn a_braked_pane_reports_freio_not_policy() {
        let s = session(
            vec![pane(
                1,
                Yurai::Automation { label: None },
                InputPolicy::Locked,
            )],
            ON,
        );
        assert_eq!(
            s.admits(PaneId(1)),
            Some(Admission::Refuse(RefusalReason::Freio)),
            "the brake is why this pane is refusing right now; saying \
             'policy' would send the operator to unlock a pane that would \
             still refuse"
        );
    }

    /// Releasing restores the EXACT prior admission — a Locked pane stays
    /// Locked, a Leader pane stays Leader. Release clears the brake; it
    /// does not set every pane free.
    #[test]
    fn releasing_restores_the_exact_prior_admission() {
        let panes = || {
            vec![
                pane(1, Yurai::Automation { label: None }, InputPolicy::Locked),
                pane(
                    2,
                    Yurai::Automation { label: None },
                    InputPolicy::Leader { id: 7 },
                ),
                pane(3, Yurai::Automation { label: None }, InputPolicy::Free),
            ]
        };
        let released = session(panes(), Freio::Released);
        assert_eq!(
            released.admits(PaneId(1)),
            Some(Admission::Refuse(RefusalReason::Policy))
        );
        assert_eq!(
            released.admits(PaneId(2)),
            Some(Admission::OnlyLeader { id: 7 })
        );
        assert_eq!(released.admits(PaneId(3)), Some(Admission::Accept));
    }

    /// The honest miss is REPORTABLE. An operator pressing a panic button
    /// must be told what it did not reach.
    #[test]
    fn the_panes_the_brake_cannot_reach_are_nameable() {
        let s = session(
            vec![
                pane(1, Yurai::Automation { label: None }, InputPolicy::Free),
                pane(2, Yurai::Unknown, InputPolicy::Free),
                pane(3, Yurai::Unknown, InputPolicy::Free),
            ],
            ON,
        );
        let missed = s.unbrakable();
        assert_eq!(
            missed,
            vec![PaneId(2), PaneId(3)],
            "silence here would let an operator believe everything stopped"
        );
    }

    #[test]
    fn a_released_session_admits_exactly_as_before_freio_existed() {
        let s = session(
            vec![pane(
                1,
                Yurai::Automation { label: None },
                InputPolicy::Free,
            )],
            Freio::Released,
        );
        assert_eq!(s.admits(PaneId(1)), Some(Admission::Accept));
    }

    #[test]
    fn an_unknown_pane_is_not_admitted_at_all() {
        let s = session(vec![], ON);
        assert_eq!(s.admits(PaneId(99)), None, "no such pane is not 'accept'");
    }
}
