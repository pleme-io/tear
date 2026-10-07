//! engate Snapshot wrap for `PaneSnapshot`.
//!
//! Centralizes the wrapper so every engate Producer over a tear pane
//! (tear-core embedded, tear-client daemon, future tear-web) produces
//! the SAME `Snap` type. Consumers (mado, ayatsuri, namimado-debug)
//! impl Consumer once and ride either backend.

use engate_types::Snapshot;

use crate::pane_snapshot::{Cell, PaneSnapshot};

/// Newtype wrapper carrying a `PaneSnapshot` through engate's typed
/// attach lifecycle. `to_ansi()` is the canonical serialization
/// consumers feed through their VT parser during `replay`.
pub struct PaneSnapshotWrap(pub PaneSnapshot);

impl Snapshot for PaneSnapshotWrap {
    fn size_bytes(&self) -> usize {
        // In-memory bytes the replay carries: visible and scrollback cells,
        // the combining table and the undecoded images. Memory, not the
        // wire: over the daemon a snapshot is CBOR at ~60 B a cell
        // (PERFORMANCE.md §2 C6, §3 class 2).
        let s = &self.0;
        let cells: usize = s.cells.iter().chain(&s.scrollback).map(Vec::len).sum();
        let marks: usize = s.combining.iter().map(Vec::len).sum();
        let images: usize = s
            .graphics
            .iter()
            .map(|g| g.params.len() + g.data.len())
            .sum();
        cells * std::mem::size_of::<Cell>() + marks * std::mem::size_of::<char>() + images
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
    use crate::graphics::{Graphic, GraphicProtocol};
    use crate::pane_snapshot::{CellAttrs, Color};

    fn dummy_snap(rows: usize, cols: usize) -> PaneSnapshot {
        PaneSnapshot::blank(rows, cols)
    }

    fn size(snap: &PaneSnapshot) -> usize {
        <PaneSnapshotWrap as Snapshot>::size_bytes(&PaneSnapshotWrap(snap.clone()))
    }

    #[test]
    fn wrap_size_bytes_scales_with_grid() {
        let small = dummy_snap(1, 1);
        let big = dummy_snap(24, 80);
        assert!(size(&big) > size(&small));
        assert_eq!(size(&big), 24 * 80 * std::mem::size_of::<Cell>());
    }

    #[test]
    fn wrap_size_bytes_counts_scrollback_marks_and_images() {
        let mut snap = dummy_snap(24, 80);
        let screen = size(&snap);
        snap.scrollback = vec![vec![Cell::BLANK; 80]; 1000];
        assert_eq!(
            size(&snap),
            screen + 1000 * 80 * std::mem::size_of::<Cell>()
        );
        let with_history = size(&snap);
        snap.combining = vec![vec!['\u{301}', '\u{302}']];
        snap.graphics = vec![Graphic {
            protocol: GraphicProtocol::Kitty,
            params: "a=T".into(),
            data: vec![0; 4096],
            at_row: 0,
            at_col: 0,
            truncated: false,
        }];
        assert_eq!(
            size(&snap),
            with_history + 2 * std::mem::size_of::<char>() + 3 + 4096
        );
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
