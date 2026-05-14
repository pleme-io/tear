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
