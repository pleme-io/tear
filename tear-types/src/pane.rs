//! Pane — the atomic unit that runs a shell and owns its PTY.

use serde::{Deserialize, Serialize};

use crate::id::PaneId;

/// One pane: the typed metadata about a running terminal session +
/// its renderable state. The actual PTY handle, terminal-state-machine
/// grid, and reader/writer tasks live in `tear-core::InProcess` —
/// this struct is the serde-friendly typed surface that crosses the
/// daemon-RPC boundary and is consumed by mado.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct TearPane {
    pub id: PaneId,
    /// Shell command executed in this pane (e.g. `"/run/current-system/sw/bin/zsh"`).
    pub shell: String,
    /// Optional arguments passed after the shell command.
    #[serde(default)]
    pub args: Vec<String>,
    /// Working directory at spawn time. `None` means inherit from the
    /// session.
    #[serde(default)]
    pub cwd: Option<String>,
    /// Environment overrides applied to this pane's child only —
    /// e.g. `TERM=xterm-ghostty`, `COLORTERM=truecolor`. Merged on top
    /// of the parent environment.
    #[serde(default)]
    pub env: Vec<(String, String)>,
    /// Current pane size in terminal cells (cols, rows). The
    /// multiplexer keeps this in sync with the actual PTY winsize.
    pub size_cells: (u16, u16),
    /// Cell at the top-left within the parent window (0, 0)-based.
    /// Computed by the layout engine; serialised so the daemon can
    /// hand mado a render-ready snapshot.
    pub origin_cells: (u16, u16),
    /// Lifecycle state.
    pub state: PaneState,
    /// Title — operator-set (via OSC 2) or derived from the running
    /// program. Drives status-bar segment rendering.
    #[serde(default)]
    pub title: String,
    /// Input acceptance policy. Default `Free` accepts input from
    /// every connected client (mado, `tear send-keys`, MCP); the
    /// daemon writes the bytes verbatim to the PTY in the order
    /// they arrive. `Locked` rejects every send_keys with a typed
    /// error — useful for "demo / observer" sessions, AI-driven
    /// panes where human input would interleave with the agent,
    /// or for the migration handoff window. New variants
    /// (`Leader`, `OwnerOnly`) land alongside Subscribe-assigned
    /// client identity in a future iteration.
    #[serde(default)]
    pub input_policy: InputPolicy,
}

/// Input acceptance policy for a single pane. Per-pane (not
/// per-client) so the daemon enforces with one lookup on every
/// `send_keys`.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum InputPolicy {
    /// Default. Every connected client's `send_keys` is written to
    /// the PTY in arrival order. Multi-renderer attach is allowed
    /// to interleave input.
    Free,
    /// Rejects every `send_keys` with `WireError::Rejected`. The
    /// pane is observe-only — every PaneBytes subscriber still
    /// gets output, but no one can type. Lifted via
    /// `Request::SetInputPolicy(pane, InputPolicy::Free)`.
    Locked,
    /// #2 — only the client whose connection identified itself with
    /// `Request::IdentifyClient(id)` matching this leader id may
    /// send keys; every other client is `Rejected`. Use case: an AI
    /// agent owns a pane and a human can watch (mado subscriber)
    /// without interleaving keystrokes. The leader id is operator-
    /// chosen — a stable 64-bit token your agent surfaces via
    /// `TEAR_CLIENT_ID`.
    ///
    /// Encoded as a struct variant so serde's internally-tagged
    /// representation can carry the integer payload (tagged
    /// representation can't hold a primitive in a tuple variant).
    Leader { id: u64 },
}

impl Default for InputPolicy {
    fn default() -> Self {
        Self::Free
    }
}

impl InputPolicy {
    /// Short label for `tear list` / `tear pane status` text output.
    #[must_use]
    pub fn label(&self) -> &'static str {
        match self {
            InputPolicy::Free => "free",
            InputPolicy::Locked => "locked",
            InputPolicy::Leader { .. } => "leader",
        }
    }

    /// Convenience constructor for `Leader { id }` so call sites
    /// don't repeat the struct-literal shape.
    #[must_use]
    pub fn leader(id: u64) -> Self {
        InputPolicy::Leader { id }
    }

    /// Returns the gating client id if this policy is `Leader`,
    /// else `None`. Cheaper than `if let` at call sites that only
    /// want to peek at the leader id.
    #[must_use]
    pub fn leader_id(&self) -> Option<u64> {
        match self {
            InputPolicy::Leader { id } => Some(*id),
            _ => None,
        }
    }
}

/// Pane lifecycle states.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PaneState {
    /// Process is alive and accepting input.
    Running,
    /// Process exited; pane stays visible until closed. tmux's
    /// `remain-on-exit` semantics.
    Exited { code: i32 },
    /// Pane was created but the child hasn't started yet (rare —
    /// only during the short window between `TearPane::spawn` and
    /// the first PTY read).
    Spawning,
}

impl Default for PaneState {
    fn default() -> Self {
        Self::Spawning
    }
}

/// Lightweight statistics surfaced by `tear list` and by the daemon's
/// status-bar refresh loop. Doesn't include the full grid contents —
/// for that the consumer reaches into `tear-core` directly.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct PaneStats {
    /// Bytes consumed by the VT parser since pane spawn. Used for
    /// `% active` displays and for sampling decisions in tier-3
    /// (mado embedding tear-core).
    pub bytes_consumed: u64,
    /// Number of complete scrollback lines pushed off the visible
    /// grid.
    pub scrollback_lines: u32,
    /// Wall-clock-seconds since the last byte arrived. The status
    /// bar can render `idle 12m` directly from this.
    pub seconds_since_last_byte: u32,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pane_state_default_is_spawning() {
        assert_eq!(PaneState::default(), PaneState::Spawning);
    }

    #[test]
    fn input_policy_leader_serialises_as_kind_tag() {
        let p = InputPolicy::Leader { id: 42 };
        let s = serde_json::to_string(&p).unwrap();
        assert!(s.contains("\"kind\":\"leader\""), "got: {s}");
        assert!(s.contains("42"), "got: {s}");
    }

    #[test]
    fn input_policy_label_covers_every_variant() {
        assert_eq!(InputPolicy::Free.label(), "free");
        assert_eq!(InputPolicy::Locked.label(), "locked");
        assert_eq!(InputPolicy::leader(7).label(), "leader");
    }

    #[test]
    fn input_policy_leader_constructor_matches_struct_form() {
        assert_eq!(InputPolicy::leader(99), InputPolicy::Leader { id: 99 });
    }

    #[test]
    fn input_policy_leader_id_returns_some_for_leader_and_none_otherwise() {
        assert_eq!(InputPolicy::leader(5).leader_id(), Some(5));
        assert_eq!(InputPolicy::Free.leader_id(), None);
        assert_eq!(InputPolicy::Locked.leader_id(), None);
    }

    #[test]
    fn input_policy_serde_round_trips_every_variant() {
        for p in [
            InputPolicy::Free,
            InputPolicy::Locked,
            InputPolicy::leader(42),
        ] {
            let json = serde_json::to_string(&p).unwrap();
            let back: InputPolicy = serde_json::from_str(&json).unwrap();
            assert_eq!(p, back, "round-trip failed for {p:?}");
        }
    }

    #[test]
    fn pane_default_fields_are_constructible() {
        let p = TearPane {
            id: PaneId(42),
            shell: "/bin/zsh".into(),
            args: vec![],
            cwd: Some("/tmp".into()),
            env: vec![],
            size_cells: (120, 40),
            origin_cells: (0, 0),
            state: PaneState::Running,
            title: "zsh".into(),
            input_policy: InputPolicy::default(),
        };
        assert_eq!(p.state, PaneState::Running);
        assert_eq!(p.size_cells, (120, 40));
        assert_eq!(p.input_policy, InputPolicy::Free);
    }

    #[test]
    fn input_policy_default_is_free() {
        assert_eq!(InputPolicy::default(), InputPolicy::Free);
    }
}
