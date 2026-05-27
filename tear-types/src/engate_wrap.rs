//! engate Snapshot wrap for `PaneSnapshot`.
//!
//! Centralizes the wrapper so every engate Producer over a tear pane
//! (tear-core embedded, tear-client daemon, future tear-web) produces
//! the SAME `Snap` type. Consumers (mado, ayatsuri, namimado-debug)
//! impl Consumer once and ride either backend.

use engate_types::Snapshot;

use crate::pane_snapshot::PaneSnapshot;

/// Newtype wrapper carrying a `PaneSnapshot` through engate's typed
/// attach lifecycle. `to_ansi()` is the canonical serialization
/// consumers feed through their VT parser during `replay`.
pub struct PaneSnapshotWrap(pub PaneSnapshot);

impl Snapshot for PaneSnapshotWrap {
    fn size_bytes(&self) -> usize {
        // Approximate — cells × 4 bytes (char + fg/bg/attrs).
        self.0.cells.iter().map(|r| r.len() * 4).sum()
    }
}

impl PaneSnapshotWrap {
    /// Borrow ANSI replay bytes — same wire-shape the daemon's
    /// engate M0 path emits as the first PaneBytes frame.
    #[must_use]
    pub fn to_ansi(&self) -> Vec<u8> {
        self.0.to_ansi()
    }

    #[must_use]
    pub fn into_inner(self) -> PaneSnapshot {
        self.0
    }
}

impl From<PaneSnapshot> for PaneSnapshotWrap {
    fn from(s: PaneSnapshot) -> Self {
        Self(s)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pane_snapshot::{Cell, CellAttrs, Color};

    fn dummy_snap(rows: usize, cols: usize) -> PaneSnapshot {
        PaneSnapshot {
            rows,
            cols,
            cells: vec![vec![Cell::BLANK; cols]; rows],
            cursor_row: 0,
            cursor_col: 0,
            alt_screen_active: false,
            cursor_visible: true,
            title: None,
            cursor_keys_mode: false,
        }
    }

    #[test]
    fn wrap_size_bytes_scales_with_grid() {
        let small = PaneSnapshotWrap(dummy_snap(1, 1));
        let big = PaneSnapshotWrap(dummy_snap(24, 80));
        assert!(<PaneSnapshotWrap as Snapshot>::size_bytes(&big)
            > <PaneSnapshotWrap as Snapshot>::size_bytes(&small));
        // 24*80 cells × 4 bytes/cell = 7680.
        assert_eq!(<PaneSnapshotWrap as Snapshot>::size_bytes(&big), 7680);
    }

    #[test]
    fn wrap_to_ansi_matches_inner_to_ansi() {
        let mut snap = dummy_snap(1, 3);
        snap.cells[0][0].ch = 'a';
        snap.cells[0][1].ch = 'b';
        snap.cells[0][2].ch = 'c';
        let wrap = PaneSnapshotWrap(snap.clone());
        assert_eq!(wrap.to_ansi(), snap.to_ansi());
    }

    #[test]
    fn from_pane_snapshot_preserves_all_fields() {
        let mut snap = dummy_snap(5, 10);
        snap.cursor_row = 2;
        snap.cursor_col = 7;
        snap.alt_screen_active = true;
        snap.cursor_visible = false;
        snap.title = Some("test-title".into());
        snap.cells[1][3].fg = Color::new(11, 22, 33);
        snap.cells[1][3].attrs = CellAttrs::BOLD;
        let wrap: PaneSnapshotWrap = snap.clone().into();
        let back = wrap.into_inner();
        assert_eq!(back.rows, 5);
        assert_eq!(back.cols, 10);
        assert_eq!(back.cursor_row, 2);
        assert_eq!(back.cursor_col, 7);
        assert!(back.alt_screen_active);
        assert!(!back.cursor_visible);
        assert_eq!(back.title.as_deref(), Some("test-title"));
        assert_eq!(back.cells[1][3].fg, Color::new(11, 22, 33));
        assert_eq!(back.cells[1][3].attrs, CellAttrs::BOLD);
    }
}
