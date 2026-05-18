//! Per-pane terminal cell grid driven by a `vte` parser.
//!
//! Phase-2-MVP scope: one `vte::Parser` + a `[rows][cols]` cell
//! buffer with cursor tracking. Just the pieces needed to render
//! `echo hi\n` correctly + a few common control sequences (CR, LF,
//! BS, CUP, EL, ED). The full VT100/xterm/Kitty surface lives in
//! mado's `terminal.rs` today; the M5 plan ([`theory/MADO-TEAR-M5.md`])
//! migrates that whole state machine HERE so both apps share one
//! parser fleet-wide. This MVP is the gravitational center the
//! migration lands on — not the final shape.
//!
//! ## What this gives Phase 2
//!
//! `InProcess::feed_pane_bytes(pane_id, bytes)` feeds bytes through
//! the per-pane parser; `InProcess::pane_snapshot(pane_id)` returns
//! a serializable [`PaneSnapshot`] that the tear-daemon ↔ tear-client
//! wire ferries to consumers. A renderer (mado, eventually) walks
//! that snapshot to draw pixels.
//!
//! ## What it deliberately does NOT do (yet)
//!
//! - SGR colors / attrs (everything is default-styled today)
//! - Alternate screen buffer
//! - Scrollback (only the visible viewport)
//! - Tab stops, scrolling regions, IRM, DECSCUSR, hyperlinks, OSC
//! - DEC mode 2026 (synchronized output)
//! - Kitty graphics, sixel
//!
//! The full surface lands when mado's `terminal.rs` MOVES here at
//! Phase 2.5 (after this MVP proves the wiring is sound).

use vte::{Params, Parser, Perform};

pub use tear_types::pane_snapshot::{Cell, PaneSnapshot};

/// Live grid + cursor + the parser that feeds them. Owns mutable
/// state, so callers wrap it in `Mutex` (the InProcess does this
/// since multiple PTY-reader threads + the RPC dispatch thread all
/// race for it).
pub struct PaneGrid {
    parser: Parser,
    state: GridState,
}

/// The grid + cursor, separated from the parser so `Perform` can
/// borrow `&mut state` while leaving the parser owner alone. (The
/// vte API has the parser CALL into a Perform impl; we use the
/// inner state as that impl.)
#[derive(Clone, Debug)]
struct GridState {
    rows: usize,
    cols: usize,
    /// `cells[row][col]`. Row-major so `cells[row]` is one screen line.
    cells: Vec<Vec<Cell>>,
    cursor_row: usize,
    cursor_col: usize,
}

impl GridState {
    fn new(cols: usize, rows: usize) -> Self {
        Self {
            rows,
            cols,
            cells: vec![vec![Cell::BLANK; cols]; rows],
            cursor_row: 0,
            cursor_col: 0,
        }
    }

    /// Scroll the grid up by one row when the cursor moves below
    /// the last row. The top row is dropped; a fresh blank row is
    /// appended. Scrollback is NOT preserved at MVP — that lands
    /// with the full terminal.rs port.
    fn scroll_up(&mut self) {
        if self.cells.is_empty() {
            return;
        }
        self.cells.remove(0);
        self.cells.push(vec![Cell::BLANK; self.cols]);
    }

    fn advance_cursor_after_print(&mut self) {
        self.cursor_col += 1;
        if self.cursor_col >= self.cols {
            // Auto-wrap: move to start of next line.
            self.cursor_col = 0;
            self.cursor_row += 1;
            if self.cursor_row >= self.rows {
                self.scroll_up();
                self.cursor_row = self.rows.saturating_sub(1);
            }
        }
    }

    fn newline(&mut self) {
        self.cursor_row += 1;
        if self.cursor_row >= self.rows {
            self.scroll_up();
            self.cursor_row = self.rows.saturating_sub(1);
        }
    }

    fn cr(&mut self) {
        self.cursor_col = 0;
    }

    fn backspace(&mut self) {
        if self.cursor_col > 0 {
            self.cursor_col -= 1;
        }
    }

    fn erase_to_end_of_line(&mut self) {
        if self.cursor_row < self.cells.len() {
            let row = &mut self.cells[self.cursor_row];
            for c in row.iter_mut().skip(self.cursor_col) {
                *c = Cell::BLANK;
            }
        }
    }

    fn erase_from_start_of_line(&mut self) {
        if self.cursor_row < self.cells.len() {
            let row = &mut self.cells[self.cursor_row];
            let stop = (self.cursor_col + 1).min(row.len());
            for c in row.iter_mut().take(stop) {
                *c = Cell::BLANK;
            }
        }
    }

    fn erase_line(&mut self) {
        if self.cursor_row < self.cells.len() {
            for c in self.cells[self.cursor_row].iter_mut() {
                *c = Cell::BLANK;
            }
        }
    }

    fn erase_all(&mut self) {
        for row in &mut self.cells {
            for c in row.iter_mut() {
                *c = Cell::BLANK;
            }
        }
    }

    fn cursor_move_relative(&mut self, drow: isize, dcol: isize) {
        let r = (self.cursor_row as isize + drow).max(0) as usize;
        let c = (self.cursor_col as isize + dcol).max(0) as usize;
        self.cursor_row = r.min(self.rows.saturating_sub(1));
        self.cursor_col = c.min(self.cols.saturating_sub(1));
    }

    fn cursor_set(&mut self, row: usize, col: usize) {
        self.cursor_row = row.min(self.rows.saturating_sub(1));
        self.cursor_col = col.min(self.cols.saturating_sub(1));
    }
}

impl Perform for GridState {
    fn print(&mut self, c: char) {
        if self.cursor_row < self.cells.len()
            && self.cursor_col < self.cells[self.cursor_row].len()
        {
            self.cells[self.cursor_row][self.cursor_col] = Cell { ch: c };
        }
        self.advance_cursor_after_print();
    }

    fn execute(&mut self, byte: u8) {
        match byte {
            b'\n' => self.newline(),
            b'\r' => self.cr(),
            b'\x08' => self.backspace(), // BS
            b'\x07' => {}                // BEL: drop (no audio at MVP)
            b'\t' => {
                // Tab to next multiple of 8 (the conventional default).
                let next = (self.cursor_col / 8 + 1) * 8;
                self.cursor_col = next.min(self.cols.saturating_sub(1));
            }
            _ => {}
        }
    }

    fn csi_dispatch(
        &mut self,
        params: &Params,
        _intermediates: &[u8],
        _ignore: bool,
        c: char,
    ) {
        // Pull the first parameter (default 1 for cursor-move,
        // default 0 for erase modes).
        let first = params
            .iter()
            .next()
            .and_then(|p| p.first().copied())
            .unwrap_or(0);
        let n = first.max(1) as isize;
        match c {
            'A' => self.cursor_move_relative(-n, 0), // CUU
            'B' => self.cursor_move_relative(n, 0),  // CUD
            'C' => self.cursor_move_relative(0, n),  // CUF
            'D' => self.cursor_move_relative(0, -n), // CUB
            'H' | 'f' => {
                // CUP / HVP: row;col (1-based)
                let mut it = params.iter();
                let row = it
                    .next()
                    .and_then(|p| p.first().copied())
                    .unwrap_or(1)
                    .max(1) as usize;
                let col = it
                    .next()
                    .and_then(|p| p.first().copied())
                    .unwrap_or(1)
                    .max(1) as usize;
                self.cursor_set(row - 1, col - 1);
            }
            'G' => {
                // CHA: cursor to column (1-based)
                let col = first.max(1) as usize - 1;
                let row = self.cursor_row;
                self.cursor_set(row, col);
            }
            'd' => {
                // VPA: cursor to row (1-based)
                let row = first.max(1) as usize - 1;
                let col = self.cursor_col;
                self.cursor_set(row, col);
            }
            'J' => {
                // ED — Erase in Display
                match first {
                    0 => {
                        self.erase_to_end_of_line();
                        for r in (self.cursor_row + 1)..self.rows {
                            for c in self.cells[r].iter_mut() {
                                *c = Cell::BLANK;
                            }
                        }
                    }
                    1 => {
                        for r in 0..self.cursor_row {
                            for c in self.cells[r].iter_mut() {
                                *c = Cell::BLANK;
                            }
                        }
                        self.erase_from_start_of_line();
                    }
                    2 | 3 => self.erase_all(),
                    _ => {}
                }
            }
            'K' => {
                // EL — Erase in Line
                match first {
                    0 => self.erase_to_end_of_line(),
                    1 => self.erase_from_start_of_line(),
                    2 => self.erase_line(),
                    _ => {}
                }
            }
            _ => {} // SGR (m), DEC private modes, etc. — out of MVP scope.
        }
    }
}

impl PaneGrid {
    #[must_use]
    pub fn new(cols: usize, rows: usize) -> Self {
        Self {
            parser: Parser::new(),
            state: GridState::new(cols, rows),
        }
    }

    /// Feed bytes from a PTY into the parser. Re-entrant per-call;
    /// callers wrap in a Mutex if multiple threads write.
    pub fn feed(&mut self, bytes: &[u8]) {
        self.parser.advance(&mut self.state, bytes);
    }

    /// Snapshot the current state. Cheap-ish — clones the cell
    /// vector. Phase-3 will offer a damage-rect API for the hot
    /// render path; today snapshots are fine for poll-based use.
    #[must_use]
    pub fn snapshot(&self) -> PaneSnapshot {
        PaneSnapshot {
            rows: self.state.rows,
            cols: self.state.cols,
            cells: self.state.cells.clone(),
            cursor_row: self.state.cursor_row,
            cursor_col: self.state.cursor_col,
        }
    }

    pub fn resize(&mut self, cols: usize, rows: usize) {
        // Naive resize: preserve top-left, truncate / pad rest.
        let mut new_cells = vec![vec![Cell::BLANK; cols]; rows];
        for r in 0..self.state.rows.min(rows) {
            for c in 0..self.state.cols.min(cols) {
                new_cells[r][c] = self.state.cells[r][c];
            }
        }
        self.state.cells = new_cells;
        self.state.rows = rows;
        self.state.cols = cols;
        self.state.cursor_row = self.state.cursor_row.min(rows.saturating_sub(1));
        self.state.cursor_col = self.state.cursor_col.min(cols.saturating_sub(1));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn print_plain_text() {
        let mut g = PaneGrid::new(10, 3);
        g.feed(b"hi");
        let snap = g.snapshot();
        assert_eq!(snap.cells[0][0].ch, 'h');
        assert_eq!(snap.cells[0][1].ch, 'i');
        assert_eq!(snap.cursor_row, 0);
        assert_eq!(snap.cursor_col, 2);
    }

    #[test]
    fn newline_advances_row() {
        let mut g = PaneGrid::new(10, 3);
        g.feed(b"hi\r\nworld");
        let snap = g.snapshot();
        assert_eq!(snap.cells[0][0].ch, 'h');
        assert_eq!(snap.cells[0][1].ch, 'i');
        assert_eq!(snap.cells[1][0].ch, 'w');
        assert_eq!(snap.cells[1][4].ch, 'd');
        assert_eq!(snap.cursor_row, 1);
        assert_eq!(snap.cursor_col, 5);
    }

    #[test]
    fn cursor_move_csi_cup() {
        let mut g = PaneGrid::new(10, 5);
        // CUP to row 3, col 5 (1-based).
        g.feed(b"\x1b[3;5H");
        let snap = g.snapshot();
        assert_eq!(snap.cursor_row, 2);
        assert_eq!(snap.cursor_col, 4);
    }

    #[test]
    fn erase_in_display_clear_all() {
        let mut g = PaneGrid::new(5, 2);
        g.feed(b"abcde\r\nfghij");
        g.feed(b"\x1b[2J");
        let snap = g.snapshot();
        for row in snap.cells {
            for cell in row {
                assert_eq!(cell.ch, ' ');
            }
        }
    }

    #[test]
    fn auto_wrap_overflows_to_next_row() {
        let mut g = PaneGrid::new(3, 3);
        g.feed(b"abcdef");
        let snap = g.snapshot();
        assert_eq!(snap.cells[0][0].ch, 'a');
        assert_eq!(snap.cells[0][2].ch, 'c');
        assert_eq!(snap.cells[1][0].ch, 'd');
        assert_eq!(snap.cells[1][2].ch, 'f');
    }

    #[test]
    fn scroll_when_cursor_passes_last_row() {
        let mut g = PaneGrid::new(3, 2);
        g.feed(b"a\r\nb\r\nc");
        let snap = g.snapshot();
        // After three lines on a 2-row grid the first ("a") scrolled out.
        assert_eq!(snap.cells[0][0].ch, 'b');
        assert_eq!(snap.cells[1][0].ch, 'c');
    }

    #[test]
    fn snapshot_text_helpers() {
        let mut g = PaneGrid::new(5, 2);
        g.feed(b"hi\r\nbye");
        let snap = g.snapshot();
        let rows = snap.to_text_rows();
        assert_eq!(rows[0], "hi   ");
        assert_eq!(rows[1], "bye  ");
        assert!(snap.to_text().contains("hi"));
        assert!(snap.to_text().contains("bye"));
    }
}
