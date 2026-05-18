//! Typed pane snapshot — the wire payload that ferries a rendered
//! pane state from `tear-core` / `tear-daemon` to a consumer.
//!
//! Lives in `tear-types` (not `tear-core`) because the wire (and
//! therefore `tear-client`) needs to deserialize these without
//! pulling in the parser. The parser side (`tear_core::PaneGrid`)
//! constructs these via `PaneGrid::snapshot()`.

use serde::{Deserialize, Serialize};

/// One cell in a snapshotted pane. Phase-2-MVP only carries the
/// rendered character; SGR colors / attrs land when mado's full
/// Cell migrates here (Phase 2.5+).
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Cell {
    pub ch: char,
}

impl Cell {
    pub const BLANK: Self = Self { ch: ' ' };
}

/// Serializable snapshot of one pane's visible grid + cursor. Sent
/// over the tear-daemon ↔ tear-client wire so consumers can render
/// without holding a reference into the live parser state.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PaneSnapshot {
    pub rows: usize,
    pub cols: usize,
    pub cells: Vec<Vec<Cell>>,
    pub cursor_row: usize,
    pub cursor_col: usize,
}

impl PaneSnapshot {
    /// Project to plain text — one String per row, blanks rendered
    /// as ASCII spaces. Useful for assertions and a future
    /// `tear list --panes` CLI surface.
    #[must_use]
    pub fn to_text_rows(&self) -> Vec<String> {
        self.cells
            .iter()
            .map(|row| row.iter().map(|c| c.ch).collect::<String>())
            .collect()
    }

    /// Joined text grid (rows separated by `\n`).
    #[must_use]
    pub fn to_text(&self) -> String {
        self.to_text_rows().join("\n")
    }
}
