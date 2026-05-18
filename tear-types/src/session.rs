//! Session — a top-level grouping of windows that survives across
//! client disconnects.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::{
    id::{PaneId, SessionId, WindowId},
    pane::TearPane,
    window::TearWindow,
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
