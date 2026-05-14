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
        };
        assert_eq!(p.state, PaneState::Running);
        assert_eq!(p.size_cells, (120, 40));
    }
}
