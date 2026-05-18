//! Per-pane terminal cell grid driven by a `vte` parser.
//!
//! Phase-2.5 scope: SGR colors (8/16/256/truecolor + bold/italic/
//! underline/etc.), alternate screen buffer, scroll regions (DECSTBM),
//! cursor save/restore (DECSC/DECRC), bounded scrollback, the usual
//! cursor-motion / erase CSI subset, and basic DEC private mode
//! toggles. Kitty graphics + sixel + hyperlinks + sync output (mode
//! 2026) + IME bracketed paste stay in mado's terminal.rs for now —
//! we lift incrementally.
//!
//! ## What this gives Phase 2 + 3
//!
//! `PaneGrid::feed(bytes)` parses; `PaneGrid::snapshot()` returns a
//! `tear_types::PaneSnapshot` ready to ship over the tear-daemon ↔
//! tear-client wire. Snapshots now carry per-cell `fg` / `bg` /
//! `attrs` so consumers can render colored output (the Phase 3 mado
//! `--tear-pane` viewer reads SGR-encoded cells directly).

use std::collections::VecDeque;

use tear_types::pane_snapshot::{
    ansi_256_color, default_ansi_palette, CellAttrs, Color,
};
use vte::{Params, Parser, Perform};

pub use tear_types::pane_snapshot::{Cell, PaneSnapshot};

/// Maximum scrollback rows kept off-screen. 1,000 rows is the
/// xterm-traditional default; consumers that want more (mado's
/// 10,000-row default) override at construction via
/// [`PaneGrid::with_scrollback`].
pub const DEFAULT_SCROLLBACK_ROWS: usize = 1_000;

/// Live grid + cursor + the parser that feeds them. Owns mutable
/// state, so callers wrap it in `Mutex` (the `InProcess` does this
/// since multiple PTY-reader threads + the RPC dispatch thread all
/// race for it).
pub struct PaneGrid {
    parser: Parser,
    pub(crate) state: GridState,
}

/// Mutable state — separated from the parser so vte's `Perform`
/// impl can borrow `&mut state` while the parser pushes bytes.
pub(crate) struct GridState {
    rows: usize,
    cols: usize,
    /// Primary screen cells.
    primary: VecDeque<Vec<Cell>>,
    /// Alternate screen cells (vim, less, htop, btop, …). Sized
    /// identically to primary; lifecycle managed by DEC mode
    /// 1049 / 47 / 1047.
    alternate: Vec<Vec<Cell>>,
    /// True when alt-screen is active.
    alt_active: bool,
    /// Bounded ring of scrollback rows that have rolled off the
    /// top of the primary screen.
    scrollback: VecDeque<Vec<Cell>>,
    scrollback_cap: usize,
    /// Cursor in 0-based (row, col) of the active screen.
    cursor_row: usize,
    cursor_col: usize,
    /// Pen state — what colors / attrs new cells inherit.
    pen_fg: Color,
    pen_bg: Color,
    pen_attrs: CellAttrs,
    /// Saved cursor + pen for DECSC / DECRC. Lazily allocated.
    saved: Option<SavedCursor>,
    /// xterm "wrap_pending" — when the last print landed in the
    /// last column, we DON'T advance the cursor immediately;
    /// instead we set this flag. The NEXT print triggers
    /// (cr + linefeed) before its own placement. CR/LF/cursor-move
    /// clear the flag without effect. This matches every real
    /// terminal — without it, `printf 'AAAAA\r\nBBBBB'` on a
    /// 5-column grid would scroll AAAAA off when \n fires.
    wrap_pending: bool,
    /// DECSTBM scroll region — inclusive top, inclusive bottom. Defaults
    /// to (0, rows-1).
    scroll_top: usize,
    scroll_bottom: usize,
    /// 16-color palette for SGR 30-37 / 40-47 / 90-97 / 100-107.
    palette: [Color; 16],
    /// Insert/Replace mode (IRM — CSI 4 h/l). When true, print
    /// shifts existing cells to the right before placement.
    insert_mode: bool,
    /// Cursor visibility (DEC mode 25 — CSI ? 25 h/l). False hides.
    cursor_visible: bool,
    /// Last printed char — REP (CSI b) repeats this.
    last_printed: Option<char>,
    /// Window / tab title (OSC 0 / OSC 2).
    title: Option<String>,
    /// OSC 133 block extractor — captures prompt + command +
    /// output + exit_code triples. Idle when the shell hasn't
    /// emitted any OSC 133 marker yet (zero blocks; on_print is
    /// a no-op). Once the shell's PS1 emits A, the extractor
    /// fills as the byte stream advances.
    pub(crate) blocks: crate::blocks::BlockExtractor,
}

#[derive(Clone, Copy)]
struct SavedCursor {
    row: usize,
    col: usize,
    fg: Color,
    bg: Color,
    attrs: CellAttrs,
}

impl GridState {
    fn new(cols: usize, rows: usize, scrollback_cap: usize) -> Self {
        Self {
            rows,
            cols,
            primary: VecDeque::from(vec![vec![Cell::BLANK; cols]; rows]),
            alternate: vec![vec![Cell::BLANK; cols]; rows],
            alt_active: false,
            scrollback: VecDeque::with_capacity(scrollback_cap.min(64)),
            scrollback_cap,
            cursor_row: 0,
            cursor_col: 0,
            pen_fg: Color::WHITE,
            pen_bg: Color::BLACK,
            pen_attrs: CellAttrs::NONE,
            saved: None,
            wrap_pending: false,
            scroll_top: 0,
            scroll_bottom: rows.saturating_sub(1),
            palette: default_ansi_palette(),
            insert_mode: false,
            cursor_visible: true,
            last_printed: None,
            title: None,
            blocks: crate::blocks::BlockExtractor::default(),
        }
    }

    /// Return a mutable reference to one cell on whichever screen
    /// is active.
    fn active_cell_mut(&mut self, row: usize, col: usize) -> Option<&mut Cell> {
        if self.alt_active {
            self.alternate.get_mut(row).and_then(|r| r.get_mut(col))
        } else {
            self.primary.get_mut(row).and_then(|r| r.get_mut(col))
        }
    }

    fn active_row_mut(&mut self, row: usize) -> Option<&mut Vec<Cell>> {
        if self.alt_active {
            self.alternate.get_mut(row)
        } else {
            self.primary.get_mut(row)
        }
    }

    fn active_rows(&self) -> impl Iterator<Item = &Vec<Cell>> + '_ {
        if self.alt_active {
            Box::new(self.alternate.iter()) as Box<dyn Iterator<Item = &Vec<Cell>>>
        } else {
            Box::new(self.primary.iter())
        }
    }

    fn blank_cell(&self) -> Cell {
        // A blank cell inherits the current background color so
        // ED/EL fill with the pen's bg (matches xterm semantics).
        Cell {
            ch: ' ',
            fg: self.pen_fg,
            bg: self.pen_bg,
            attrs: CellAttrs::NONE,
        }
    }

    fn current_cell_for_print(&self, ch: char) -> Cell {
        Cell {
            ch,
            fg: self.pen_fg,
            bg: self.pen_bg,
            attrs: self.pen_attrs,
        }
    }

    fn scroll_region_up(&mut self) {
        // Scroll within [scroll_top, scroll_bottom]. Bottom row
        // gets a blank; top row is pushed to scrollback (only when
        // primary screen + full-screen region).
        if self.scroll_top > self.scroll_bottom {
            return;
        }
        let blank = vec![self.blank_cell(); self.cols];
        let full_region = self.scroll_top == 0 && self.scroll_bottom == self.rows.saturating_sub(1);
        if self.alt_active {
            if self.scroll_top < self.alternate.len() {
                self.alternate.remove(self.scroll_top);
                self.alternate
                    .insert(self.scroll_bottom.min(self.alternate.len()), blank);
            }
        } else {
            if full_region {
                if let Some(top) = self.primary.pop_front() {
                    if self.scrollback_cap > 0 {
                        if self.scrollback.len() >= self.scrollback_cap {
                            self.scrollback.pop_front();
                        }
                        self.scrollback.push_back(top);
                    }
                }
                self.primary.push_back(blank);
            } else if self.scroll_top < self.primary.len() {
                self.primary.remove(self.scroll_top);
                let insert_at = (self.scroll_bottom + 1).min(self.primary.len());
                self.primary.insert(insert_at, blank);
            }
        }
    }

    fn advance_cursor_after_print(&mut self) {
        if self.cursor_col + 1 >= self.cols {
            // Defer wrap — leave cursor at last col, set flag.
            // Next print will fire (cr + linefeed) before placing.
            self.wrap_pending = true;
        } else {
            self.cursor_col += 1;
        }
    }

    fn linefeed(&mut self) {
        if self.cursor_row == self.scroll_bottom {
            self.scroll_region_up();
        } else if self.cursor_row + 1 < self.rows {
            self.cursor_row += 1;
        }
    }

    fn carriage_return(&mut self) {
        self.cursor_col = 0;
    }

    fn backspace(&mut self) {
        if self.cursor_col > 0 {
            self.cursor_col -= 1;
        }
    }

    fn tab_forward(&mut self) {
        let next = ((self.cursor_col / 8) + 1) * 8;
        self.cursor_col = next.min(self.cols.saturating_sub(1));
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

    fn erase_to_end_of_line(&mut self) {
        let row = self.cursor_row;
        let start = self.cursor_col;
        let blank = self.blank_cell();
        if let Some(r) = self.active_row_mut(row) {
            for c in r.iter_mut().skip(start) {
                *c = blank;
            }
        }
    }

    fn erase_from_start_of_line(&mut self) {
        let row = self.cursor_row;
        let stop = self.cursor_col + 1;
        let blank = self.blank_cell();
        if let Some(r) = self.active_row_mut(row) {
            let stop = stop.min(r.len());
            for c in r.iter_mut().take(stop) {
                *c = blank;
            }
        }
    }

    fn erase_line(&mut self) {
        let row = self.cursor_row;
        let blank = self.blank_cell();
        if let Some(r) = self.active_row_mut(row) {
            for c in r.iter_mut() {
                *c = blank;
            }
        }
    }

    fn erase_below_cursor(&mut self) {
        // ED(0): from cursor to end of screen.
        self.erase_to_end_of_line();
        let start = self.cursor_row + 1;
        let end = self.rows;
        let blank = self.blank_cell();
        for r in start..end {
            if let Some(row) = self.active_row_mut(r) {
                for c in row.iter_mut() {
                    *c = blank;
                }
            }
        }
    }

    fn erase_above_cursor(&mut self) {
        // ED(1): from start of screen to cursor (inclusive).
        let stop_row = self.cursor_row;
        let blank = self.blank_cell();
        for r in 0..stop_row {
            if let Some(row) = self.active_row_mut(r) {
                for c in row.iter_mut() {
                    *c = blank;
                }
            }
        }
        self.erase_from_start_of_line();
    }

    fn erase_all(&mut self) {
        let blank = self.blank_cell();
        let rows = self.rows;
        for r in 0..rows {
            if let Some(row) = self.active_row_mut(r) {
                for c in row.iter_mut() {
                    *c = blank;
                }
            }
        }
    }

    fn save_cursor(&mut self) {
        self.saved = Some(SavedCursor {
            row: self.cursor_row,
            col: self.cursor_col,
            fg: self.pen_fg,
            bg: self.pen_bg,
            attrs: self.pen_attrs,
        });
    }

    fn restore_cursor(&mut self) {
        if let Some(s) = self.saved {
            self.cursor_row = s.row.min(self.rows.saturating_sub(1));
            self.cursor_col = s.col.min(self.cols.saturating_sub(1));
            self.pen_fg = s.fg;
            self.pen_bg = s.bg;
            self.pen_attrs = s.attrs;
        }
    }

    fn enter_alt_screen(&mut self, clear: bool) {
        if !self.alt_active {
            self.alt_active = true;
        }
        if clear {
            for row in &mut self.alternate {
                for c in row.iter_mut() {
                    *c = Cell::BLANK;
                }
            }
            self.cursor_row = 0;
            self.cursor_col = 0;
        }
    }

    fn leave_alt_screen(&mut self) {
        self.alt_active = false;
    }

    // ── SGR ────────────────────────────────────────────────────

    fn apply_sgr(&mut self, params: &Params) {
        // SGR params can include sub-params for 38/48;5;n and
        // 38/48;2;r;g;b. We flatten + walk.
        let flat: Vec<u16> = params
            .iter()
            .flat_map(|p| p.iter().copied())
            .collect();
        if flat.is_empty() {
            self.sgr_reset();
            return;
        }
        let mut i = 0;
        while i < flat.len() {
            let p = flat[i];
            match p {
                0 => self.sgr_reset(),
                1 => self.pen_attrs.insert(CellAttrs::BOLD),
                2 => self.pen_attrs.insert(CellAttrs::DIM),
                3 => self.pen_attrs.insert(CellAttrs::ITALIC),
                4 => self.pen_attrs.insert(CellAttrs::UNDERLINE),
                5 | 6 => self.pen_attrs.insert(CellAttrs::BLINK),
                7 => self.pen_attrs.insert(CellAttrs::INVERSE),
                8 => self.pen_attrs.insert(CellAttrs::HIDDEN),
                9 => self.pen_attrs.insert(CellAttrs::STRIKETHROUGH),
                21 | 22 => {
                    self.pen_attrs.remove(CellAttrs::BOLD);
                    self.pen_attrs.remove(CellAttrs::DIM);
                }
                23 => self.pen_attrs.remove(CellAttrs::ITALIC),
                24 => self.pen_attrs.remove(CellAttrs::UNDERLINE),
                25 => self.pen_attrs.remove(CellAttrs::BLINK),
                27 => self.pen_attrs.remove(CellAttrs::INVERSE),
                28 => self.pen_attrs.remove(CellAttrs::HIDDEN),
                29 => self.pen_attrs.remove(CellAttrs::STRIKETHROUGH),
                30..=37 => self.pen_fg = self.palette[(p - 30) as usize],
                38 => {
                    // 38;5;n (256) or 38;2;r;g;b (truecolor)
                    if let Some(c) = self.parse_extended_color(&flat, &mut i) {
                        self.pen_fg = c;
                    }
                }
                39 => self.pen_fg = Color::WHITE,
                40..=47 => self.pen_bg = self.palette[(p - 40) as usize],
                48 => {
                    if let Some(c) = self.parse_extended_color(&flat, &mut i) {
                        self.pen_bg = c;
                    }
                }
                49 => self.pen_bg = Color::BLACK,
                90..=97 => self.pen_fg = self.palette[8 + (p - 90) as usize],
                100..=107 => self.pen_bg = self.palette[8 + (p - 100) as usize],
                _ => {} // unknown — drop
            }
            i += 1;
        }
    }

    fn sgr_reset(&mut self) {
        self.pen_fg = Color::WHITE;
        self.pen_bg = Color::BLACK;
        self.pen_attrs = CellAttrs::NONE;
    }

    /// Parse the extended-color params that follow a 38 or 48
    /// directive. `i` points at the 38/48; advances it past the
    /// consumed sub-params on success. Returns None on malformed
    /// input.
    fn parse_extended_color(&self, flat: &[u16], i: &mut usize) -> Option<Color> {
        let mode = *flat.get(*i + 1)?;
        match mode {
            5 => {
                let n = *flat.get(*i + 2)?;
                *i += 2;
                Some(ansi_256_color(n, &self.palette))
            }
            2 => {
                let r = *flat.get(*i + 2)? as u8;
                let g = *flat.get(*i + 3)? as u8;
                let b = *flat.get(*i + 4)? as u8;
                *i += 4;
                Some(Color::new(r, g, b))
            }
            _ => None,
        }
    }
}

impl Perform for GridState {
    fn print(&mut self, c: char) {
        // Pane-as-block: feed the extractor BEFORE placement so
        // its phase state reflects the same chronology the
        // grid sees. Cheap when the extractor is Idle (single
        // Option-is-none check).
        self.blocks.on_print(c);
        // Honour deferred wrap from the previous print, then place.
        if self.wrap_pending {
            self.wrap_pending = false;
            self.cursor_col = 0;
            self.linefeed();
        }
        let cell = self.current_cell_for_print(c);
        let row = self.cursor_row;
        let col = self.cursor_col;
        if self.insert_mode {
            // IRM: shift cells in the current row right starting at
            // cursor; drop the last cell to make room.
            let cols = self.cols;
            if let Some(r) = self.active_row_mut(row) {
                if col < r.len() {
                    r.insert(col, cell);
                    r.truncate(cols);
                }
            }
        } else if let Some(slot) = self.active_cell_mut(row, col) {
            *slot = cell;
        }
        self.last_printed = Some(c);
        self.advance_cursor_after_print();
    }

    fn execute(&mut self, byte: u8) {
        // Any control byte cancels a pending wrap — the cursor's
        // about to be moved or text deferred elsewhere.
        self.wrap_pending = false;
        match byte {
            b'\n' => self.linefeed(),
            b'\r' => self.carriage_return(),
            b'\x08' => self.backspace(),
            b'\t' => self.tab_forward(),
            b'\x07' => {} // BEL
            _ => {}
        }
    }

    fn csi_dispatch(
        &mut self,
        params: &Params,
        intermediates: &[u8],
        _ignore: bool,
        c: char,
    ) {
        // CSI sequences other than pure-SGR clear a pending wrap.
        if c != 'm' {
            self.wrap_pending = false;
        }
        let first = params
            .iter()
            .next()
            .and_then(|p| p.first().copied())
            .unwrap_or(0);
        let n = first.max(1) as isize;
        // DEC private (CSI ? ... h/l) mode toggles — recognised by
        // the leading '?' intermediate.
        if intermediates.first() == Some(&b'?') && (c == 'h' || c == 'l') {
            let set = c == 'h';
            for p in params.iter() {
                if let Some(&code) = p.first() {
                    self.apply_dec_mode(code, set);
                }
            }
            return;
        }
        match c {
            'A' => self.cursor_move_relative(-n, 0),
            'B' => self.cursor_move_relative(n, 0),
            'C' => self.cursor_move_relative(0, n),
            'D' => self.cursor_move_relative(0, -n),
            'E' => {
                self.carriage_return();
                self.cursor_move_relative(n, 0);
            }
            'F' => {
                self.carriage_return();
                self.cursor_move_relative(-n, 0);
            }
            'G' => {
                let col = first.max(1) as usize - 1;
                let row = self.cursor_row;
                self.cursor_set(row, col);
            }
            'H' | 'f' => {
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
            'J' => match first {
                0 => self.erase_below_cursor(),
                1 => self.erase_above_cursor(),
                2 | 3 => self.erase_all(),
                _ => {}
            },
            'K' => match first {
                0 => self.erase_to_end_of_line(),
                1 => self.erase_from_start_of_line(),
                2 => self.erase_line(),
                _ => {}
            },
            'L' => {
                // IL — Insert Line. Inserts N blank lines at cursor;
                // pushes lines below down (and off the bottom of region).
                let blank = vec![self.blank_cell(); self.cols];
                let row = self.cursor_row;
                for _ in 0..n {
                    if self.alt_active {
                        if row < self.alternate.len() && row <= self.scroll_bottom {
                            self.alternate.insert(row, blank.clone());
                            if self.scroll_bottom + 1 < self.alternate.len() {
                                self.alternate.remove(self.scroll_bottom + 1);
                            }
                        }
                    } else if row < self.primary.len() && row <= self.scroll_bottom {
                        self.primary.insert(row, blank.clone());
                        if self.scroll_bottom + 1 < self.primary.len() {
                            self.primary.remove(self.scroll_bottom + 1);
                        }
                    }
                }
            }
            'M' => {
                // DL — Delete Line. Removes N lines at cursor; pulls
                // lines below up; pads with blanks at region bottom.
                let blank = vec![self.blank_cell(); self.cols];
                let row = self.cursor_row;
                for _ in 0..n {
                    if self.alt_active {
                        if row < self.alternate.len() && row <= self.scroll_bottom {
                            self.alternate.remove(row);
                            let insert_at = (self.scroll_bottom).min(self.alternate.len());
                            self.alternate.insert(insert_at, blank.clone());
                        }
                    } else if row < self.primary.len() && row <= self.scroll_bottom {
                        self.primary.remove(row);
                        let insert_at = (self.scroll_bottom).min(self.primary.len());
                        self.primary.insert(insert_at, blank.clone());
                    }
                }
            }
            '@' => {
                // ICH — Insert N blank cells at cursor; shifts right.
                let blank = self.blank_cell();
                let row = self.cursor_row;
                let col = self.cursor_col;
                let cols = self.cols;
                if let Some(r) = self.active_row_mut(row) {
                    for _ in 0..n {
                        if col < r.len() {
                            r.insert(col, blank);
                            r.truncate(cols);
                        }
                    }
                }
            }
            'P' => {
                // DCH — Delete N cells at cursor; pulls remainder of row left.
                let blank = self.blank_cell();
                let row = self.cursor_row;
                let col = self.cursor_col;
                let cols = self.cols;
                if let Some(r) = self.active_row_mut(row) {
                    for _ in 0..n {
                        if col < r.len() {
                            r.remove(col);
                            r.push(blank);
                            if r.len() > cols {
                                r.truncate(cols);
                            }
                        }
                    }
                }
            }
            'X' => {
                // ECH — Erase N cells at cursor in place (no shift).
                let blank = self.blank_cell();
                let row = self.cursor_row;
                let col = self.cursor_col;
                let n_usize = n as usize;
                if let Some(r) = self.active_row_mut(row) {
                    for i in 0..n_usize {
                        if col + i < r.len() {
                            r[col + i] = blank;
                        }
                    }
                }
            }
            'b' => {
                // REP — Repeat last printed char N times.
                if let Some(c) = self.last_printed {
                    for _ in 0..n {
                        Perform::print(self, c);
                    }
                }
            }
            'h' => {
                // SM — set mode. Currently support IRM (4).
                for p in params.iter() {
                    if p.first().copied() == Some(4) {
                        self.insert_mode = true;
                    }
                }
            }
            'l' => {
                // RM — reset mode. Currently support IRM (4).
                for p in params.iter() {
                    if p.first().copied() == Some(4) {
                        self.insert_mode = false;
                    }
                }
            }
            'S' => {
                for _ in 0..n {
                    self.scroll_region_up();
                }
            }
            'T' => {
                // SD — scroll down (reverse). Insert blank rows at top.
                for _ in 0..n {
                    let blank = vec![self.blank_cell(); self.cols];
                    if self.alt_active {
                        if self.scroll_top < self.alternate.len() {
                            self.alternate
                                .insert(self.scroll_top, blank);
                            if self.scroll_bottom + 1 < self.alternate.len() {
                                self.alternate.remove(self.scroll_bottom + 1);
                            }
                        }
                    } else if self.scroll_top < self.primary.len() {
                        self.primary.insert(self.scroll_top, blank);
                        if self.scroll_bottom + 1 < self.primary.len() {
                            self.primary.remove(self.scroll_bottom + 1);
                        }
                    }
                }
            }
            'd' => {
                let row = first.max(1) as usize - 1;
                let col = self.cursor_col;
                self.cursor_set(row, col);
            }
            'm' => self.apply_sgr(params),
            'r' => {
                // DECSTBM — set scroll region
                let mut it = params.iter();
                let top = it
                    .next()
                    .and_then(|p| p.first().copied())
                    .unwrap_or(1)
                    .max(1) as usize
                    - 1;
                let bottom = it
                    .next()
                    .and_then(|p| p.first().copied())
                    .unwrap_or(self.rows as u16)
                    .max(1) as usize
                    - 1;
                self.scroll_top = top.min(self.rows.saturating_sub(1));
                self.scroll_bottom = bottom.min(self.rows.saturating_sub(1));
                self.cursor_set(0, 0);
            }
            's' => self.save_cursor(),
            'u' => self.restore_cursor(),
            _ => {}
        }
    }

    fn osc_dispatch(&mut self, params: &[&[u8]], _bell_terminated: bool) {
        // OSC 0 / 1 / 2 — set window/icon title. We treat them
        // identically: title is the second param decoded as UTF-8.
        let code = params.first().and_then(|p| std::str::from_utf8(p).ok());
        if matches!(code, Some("0") | Some("1") | Some("2")) {
            if let Some(t) = params.get(1).and_then(|p| std::str::from_utf8(p).ok()) {
                self.title = Some(t.to_owned());
            }
            return;
        }
        // OSC 133 — FinalTerm prompt marks. Drive the block
        // extractor. The marker (A/B/C/D[;<n>]) lives in
        // params[1] for the standard `OSC 133;A` shape but
        // some shells emit `OSC 133;A;…` with extra fields
        // after — we pass the whole remainder to the extractor.
        if matches!(code, Some("133")) {
            // Rebuild the marker as "A" / "B;extra" / "D;0" etc.
            let marker: String = params
                .iter()
                .skip(1)
                .filter_map(|p| std::str::from_utf8(p).ok())
                .collect::<Vec<_>>()
                .join(";");
            self.blocks.on_osc_133(&marker);
        }
    }

    fn esc_dispatch(&mut self, _intermediates: &[u8], _ignore: bool, byte: u8) {
        match byte {
            b'7' => self.save_cursor(),
            b'8' => self.restore_cursor(),
            b'D' => self.linefeed(),
            b'E' => {
                self.linefeed();
                self.carriage_return();
            }
            b'M' => {
                // RI — reverse index. Move cursor up; scroll down if at top.
                if self.cursor_row == self.scroll_top {
                    // Insert a blank at top, drop bottom.
                    let blank = vec![self.blank_cell(); self.cols];
                    if self.alt_active {
                        if self.scroll_top < self.alternate.len() {
                            self.alternate.insert(self.scroll_top, blank);
                            if self.scroll_bottom + 1 < self.alternate.len() {
                                self.alternate.remove(self.scroll_bottom + 1);
                            }
                        }
                    } else {
                        self.primary.insert(self.scroll_top, blank);
                        if self.scroll_bottom + 1 < self.primary.len() {
                            self.primary.remove(self.scroll_bottom + 1);
                        }
                    }
                } else if self.cursor_row > 0 {
                    self.cursor_row -= 1;
                }
            }
            b'c' => {
                // RIS — Reset to Initial State.
                self.sgr_reset();
                self.erase_all();
                self.cursor_set(0, 0);
                self.scroll_top = 0;
                self.scroll_bottom = self.rows.saturating_sub(1);
                self.alt_active = false;
                self.saved = None;
            }
            _ => {}
        }
    }
}

impl GridState {
    fn apply_dec_mode(&mut self, code: u16, set: bool) {
        match code {
            // 47 / 1047 / 1049 — alternate screen variants. Differences:
            //   47   : enter/leave alt buffer; no cursor save.
            //   1047 : like 47 but clears alt buffer on enter.
            //   1049 : 1047 + DECSC/DECRC save+restore cursor.
            47 => {
                if set {
                    self.enter_alt_screen(false);
                } else {
                    self.leave_alt_screen();
                }
            }
            1047 => {
                if set {
                    self.enter_alt_screen(true);
                } else {
                    self.erase_all();
                    self.leave_alt_screen();
                }
            }
            1049 => {
                if set {
                    self.save_cursor();
                    self.enter_alt_screen(true);
                } else {
                    self.erase_all();
                    self.leave_alt_screen();
                    self.restore_cursor();
                }
            }
            25 => self.cursor_visible = set, // DECTCEM
            _ => {} // Autowrap, bracketed-paste, mouse modes etc. land later.
        }
    }
}

impl PaneGrid {
    #[must_use]
    pub fn new(cols: usize, rows: usize) -> Self {
        Self::with_scrollback(cols, rows, DEFAULT_SCROLLBACK_ROWS)
    }

    #[must_use]
    pub fn with_scrollback(cols: usize, rows: usize, scrollback_cap: usize) -> Self {
        Self {
            parser: Parser::new(),
            state: GridState::new(cols, rows, scrollback_cap),
        }
    }

    pub fn feed(&mut self, bytes: &[u8]) {
        self.parser.advance(&mut self.state, bytes);
    }

    #[must_use]
    pub fn snapshot(&self) -> PaneSnapshot {
        let cells: Vec<Vec<Cell>> = self.state.active_rows().cloned().collect();
        PaneSnapshot {
            rows: self.state.rows,
            cols: self.state.cols,
            cells,
            cursor_row: self.state.cursor_row,
            cursor_col: self.state.cursor_col,
            alt_screen_active: self.state.alt_active,
            cursor_visible: self.state.cursor_visible,
            title: self.state.title.clone(),
        }
    }

    /// Current window title (OSC 0 / 2). None until the first
    /// title set; cleared on RIS.
    #[must_use]
    pub fn title(&self) -> Option<&str> {
        self.state.title.as_deref()
    }

    /// Number of scrollback rows that have rolled off the primary
    /// screen. Useful for tests + UI affordances.
    #[must_use]
    pub fn scrollback_len(&self) -> usize {
        self.state.scrollback.len()
    }

    pub fn resize(&mut self, cols: usize, rows: usize) {
        // Naive resize: preserve top-left, truncate / pad rest. The
        // full reflow algorithm (mado's grid-reflow.rs) lands when
        // we port the rest of the terminal state machine.
        let mut new_primary: VecDeque<Vec<Cell>> = VecDeque::with_capacity(rows);
        for r in 0..rows {
            let mut new_row = vec![Cell::BLANK; cols];
            if let Some(existing) = self.state.primary.get(r) {
                let n = existing.len().min(cols);
                new_row[..n].copy_from_slice(&existing[..n]);
            }
            new_primary.push_back(new_row);
        }
        let mut new_alt = vec![vec![Cell::BLANK; cols]; rows];
        for r in 0..rows.min(self.state.alternate.len()) {
            let existing = &self.state.alternate[r];
            let n = existing.len().min(cols);
            new_alt[r][..n].copy_from_slice(&existing[..n]);
        }
        self.state.primary = new_primary;
        self.state.alternate = new_alt;
        self.state.rows = rows;
        self.state.cols = cols;
        self.state.cursor_row = self.state.cursor_row.min(rows.saturating_sub(1));
        self.state.cursor_col = self.state.cursor_col.min(cols.saturating_sub(1));
        self.state.scroll_top = 0;
        self.state.scroll_bottom = rows.saturating_sub(1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tear_types::pane_snapshot::{CellAttrs, Color};

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
        assert_eq!(snap.cells[1][0].ch, 'w');
        assert_eq!(snap.cursor_row, 1);
        assert_eq!(snap.cursor_col, 5);
    }

    #[test]
    fn cursor_move_csi_cup() {
        let mut g = PaneGrid::new(10, 5);
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
        assert_eq!(snap.cells[0][2].ch, 'c');
        assert_eq!(snap.cells[1][0].ch, 'd');
    }

    #[test]
    fn scroll_into_scrollback_on_overflow() {
        let mut g = PaneGrid::with_scrollback(3, 2, 100);
        g.feed(b"a\r\nb\r\nc");
        let snap = g.snapshot();
        // First row scrolled off; "b" is on row 0, "c" on row 1.
        assert_eq!(snap.cells[0][0].ch, 'b');
        assert_eq!(snap.cells[1][0].ch, 'c');
        assert!(g.scrollback_len() >= 1);
    }

    #[test]
    fn sgr_red_foreground_sticks_through_a_word() {
        let mut g = PaneGrid::new(10, 1);
        g.feed(b"\x1b[31mRED\x1b[0m");
        let snap = g.snapshot();
        let red = tear_types::pane_snapshot::ANSI_COLORS[1];
        assert_eq!(snap.cells[0][0].ch, 'R');
        assert_eq!(snap.cells[0][0].fg, red);
        assert_eq!(snap.cells[0][1].fg, red);
        assert_eq!(snap.cells[0][2].fg, red);
    }

    #[test]
    fn sgr_truecolor_fg() {
        let mut g = PaneGrid::new(10, 1);
        g.feed(b"\x1b[38;2;200;100;50mORANGE");
        let snap = g.snapshot();
        assert_eq!(snap.cells[0][0].fg, Color::new(200, 100, 50));
        assert_eq!(snap.cells[0][5].fg, Color::new(200, 100, 50));
    }

    #[test]
    fn sgr_256_color_index() {
        let mut g = PaneGrid::new(10, 1);
        g.feed(b"\x1b[38;5;196mX");
        let snap = g.snapshot();
        // 196 in the 256-palette = bright red-ish (R idx 5 G 0 B 0)
        assert!(snap.cells[0][0].fg.r > 200);
    }

    #[test]
    fn sgr_bold_attr_sticks() {
        let mut g = PaneGrid::new(10, 1);
        g.feed(b"\x1b[1mBOLD");
        let snap = g.snapshot();
        assert!(snap.cells[0][0].attrs.contains(CellAttrs::BOLD));
    }

    #[test]
    fn sgr_reset_returns_default_pen() {
        let mut g = PaneGrid::new(10, 1);
        g.feed(b"\x1b[31m\x1b[0mX");
        let snap = g.snapshot();
        assert_eq!(snap.cells[0][0].fg, Color::WHITE);
    }

    #[test]
    fn alt_screen_isolates_writes_and_preserves_primary() {
        let mut g = PaneGrid::new(5, 2);
        g.feed(b"AAAAA\r\nBBBBB");
        // Enter alt-screen via DEC mode 1049.
        g.feed(b"\x1b[?1049h");
        // Should be on a cleared alt buffer.
        let alt_snap = g.snapshot();
        assert!(alt_snap.alt_screen_active);
        assert_eq!(alt_snap.cells[0][0].ch, ' ');
        // Write something on alt.
        g.feed(b"ZZZZZ");
        // Leave alt-screen — primary should still hold AAAAA / BBBBB.
        g.feed(b"\x1b[?1049l");
        let primary_snap = g.snapshot();
        assert!(!primary_snap.alt_screen_active);
        assert_eq!(primary_snap.cells[0][0].ch, 'A');
        assert_eq!(primary_snap.cells[1][0].ch, 'B');
    }

    #[test]
    fn save_restore_cursor_via_decsc_decrc() {
        let mut g = PaneGrid::new(10, 5);
        g.feed(b"\x1b[3;5H");
        g.feed(b"\x1b7"); // DECSC
        g.feed(b"\x1b[1;1H");
        g.feed(b"\x1b8"); // DECRC — restore
        let snap = g.snapshot();
        assert_eq!(snap.cursor_row, 2);
        assert_eq!(snap.cursor_col, 4);
    }

    #[test]
    fn snapshot_text_helpers() {
        let mut g = PaneGrid::new(5, 2);
        g.feed(b"hi\r\nbye");
        let snap = g.snapshot();
        let rows = snap.to_text_rows();
        assert_eq!(rows[0], "hi   ");
        assert_eq!(rows[1], "bye  ");
    }

    #[test]
    fn osc_2_sets_window_title() {
        let mut g = PaneGrid::new(10, 1);
        g.feed(b"\x1b]2;hello world\x07");
        assert_eq!(g.title(), Some("hello world"));
        let snap = g.snapshot();
        assert_eq!(snap.title.as_deref(), Some("hello world"));
    }

    #[test]
    fn dec_25_hides_cursor() {
        let mut g = PaneGrid::new(10, 1);
        let snap_before = g.snapshot();
        assert!(snap_before.cursor_visible);
        g.feed(b"\x1b[?25l");
        let snap_hidden = g.snapshot();
        assert!(!snap_hidden.cursor_visible);
        g.feed(b"\x1b[?25h");
        let snap_back = g.snapshot();
        assert!(snap_back.cursor_visible);
    }

    #[test]
    fn ich_inserts_cells_and_shifts_right() {
        let mut g = PaneGrid::new(6, 1);
        g.feed(b"abcdef");
        g.feed(b"\x1b[1;1H"); // cursor to (0,0)
        g.feed(b"\x1b[2@"); // ICH 2 — insert 2 blanks at cursor
        let snap = g.snapshot();
        assert_eq!(snap.cells[0][0].ch, ' ');
        assert_eq!(snap.cells[0][1].ch, ' ');
        assert_eq!(snap.cells[0][2].ch, 'a');
        assert_eq!(snap.cells[0][3].ch, 'b');
    }

    #[test]
    fn dch_deletes_cells_and_shifts_left() {
        let mut g = PaneGrid::new(6, 1);
        g.feed(b"abcdef");
        g.feed(b"\x1b[1;2H"); // cursor to (0,1) — on 'b'
        g.feed(b"\x1b[2P"); // DCH 2
        let snap = g.snapshot();
        assert_eq!(snap.cells[0][0].ch, 'a');
        assert_eq!(snap.cells[0][1].ch, 'd');
        assert_eq!(snap.cells[0][2].ch, 'e');
        assert_eq!(snap.cells[0][3].ch, 'f');
    }

    #[test]
    fn ech_erases_in_place() {
        let mut g = PaneGrid::new(6, 1);
        g.feed(b"abcdef");
        g.feed(b"\x1b[1;2H");
        g.feed(b"\x1b[2X"); // ECH 2 — erase 2 cells in place
        let snap = g.snapshot();
        assert_eq!(snap.cells[0][0].ch, 'a');
        assert_eq!(snap.cells[0][1].ch, ' ');
        assert_eq!(snap.cells[0][2].ch, ' ');
        assert_eq!(snap.cells[0][3].ch, 'd');
    }

    #[test]
    fn il_dl_insert_delete_line() {
        let mut g = PaneGrid::new(3, 4);
        g.feed(b"AAA\r\nBBB\r\nCCC\r\nDDD");
        g.feed(b"\x1b[2;1H"); // cursor to row 2
        g.feed(b"\x1b[1L"); // IL 1 — insert blank line above
        let snap1 = g.snapshot();
        // After IL: row 0 unchanged (AAA), row 1 blank, then BBB, CCC.
        // DDD pushed off the bottom of region.
        assert_eq!(snap1.cells[0][0].ch, 'A');
        assert_eq!(snap1.cells[1][0].ch, ' ');
        assert_eq!(snap1.cells[2][0].ch, 'B');
        // DL the inserted blank.
        g.feed(b"\x1b[1M"); // DL 1
        let snap2 = g.snapshot();
        assert_eq!(snap2.cells[1][0].ch, 'B');
    }

    #[test]
    fn rep_repeats_last_printable_char() {
        let mut g = PaneGrid::new(10, 1);
        g.feed(b"X\x1b[5b"); // print X, then REP 5
        let snap = g.snapshot();
        for c in 0..6 {
            assert_eq!(snap.cells[0][c].ch, 'X', "col {c}");
        }
    }

    #[test]
    fn irm_inserts_on_print() {
        let mut g = PaneGrid::new(6, 1);
        g.feed(b"abcdef");
        g.feed(b"\x1b[1;1H"); // cursor home
        g.feed(b"\x1b[4hZ"); // SM 4 (IRM on), then print Z
        let snap = g.snapshot();
        assert_eq!(snap.cells[0][0].ch, 'Z');
        assert_eq!(snap.cells[0][1].ch, 'a');
        assert_eq!(snap.cells[0][2].ch, 'b');
    }

    #[test]
    fn ri_scrolls_down_at_top_of_region() {
        let mut g = PaneGrid::new(3, 3);
        g.feed(b"a\r\nb\r\nc"); // 3 lines
        g.feed(b"\x1b[1;1H"); // cursor to top
        g.feed(b"\x1bM"); // RI — should insert blank row at top
        let snap = g.snapshot();
        assert_eq!(snap.cells[0][0].ch, ' ');
        assert_eq!(snap.cells[1][0].ch, 'a');
    }

    #[test]
    fn resize_preserves_top_left_content() {
        let mut g = PaneGrid::new(5, 3);
        g.feed(b"HELLO\r\nWORLD\r\nTHERE");
        // Shrink to 4x2 — top-left HELLO[0..4] + WORLD[0..4] survive.
        g.resize(4, 2);
        let snap = g.snapshot();
        assert_eq!(snap.cols, 4);
        assert_eq!(snap.rows, 2);
        assert_eq!(snap.cells[0][0].ch, 'H');
        assert_eq!(snap.cells[0][3].ch, 'L');
        assert_eq!(snap.cells[1][0].ch, 'W');
        // Cursor was at (2, 5) before shrink — should clamp to (1, 3).
        assert_eq!(snap.cursor_row, 1);
        assert_eq!(snap.cursor_col, 3);
    }

    #[test]
    fn resize_grow_pads_with_blanks() {
        let mut g = PaneGrid::new(3, 2);
        g.feed(b"AB\r\nCD");
        g.resize(5, 4);
        let snap = g.snapshot();
        assert_eq!(snap.cols, 5);
        assert_eq!(snap.rows, 4);
        assert_eq!(snap.cells[0][0].ch, 'A');
        assert_eq!(snap.cells[0][3].ch, ' ');
        assert_eq!(snap.cells[2][0].ch, ' ');
    }

    #[test]
    fn scrollback_caps_at_configured_size() {
        let mut g = PaneGrid::with_scrollback(3, 2, 3);
        // Push 10 lines through; scrollback should cap at 3.
        for i in 0..10u8 {
            g.feed(&[b'a' + i, b'\r', b'\n']);
        }
        assert!(g.scrollback_len() <= 3);
    }

    // ── SGR / wire-format edge cases ──────────────────────────

    #[test]
    fn sgr_truecolor_with_missing_params_does_not_panic() {
        // Only 2 of the 5 expected params for 38;2;R;G;B.
        let mut g = PaneGrid::new(5, 1);
        g.feed(b"\x1b[38;2;200mX");
        // Should not panic; pen unchanged or defaults.
        let snap = g.snapshot();
        assert_eq!(snap.cells[0][0].ch, 'X');
    }

    #[test]
    fn sgr_256_with_missing_index_does_not_panic() {
        let mut g = PaneGrid::new(5, 1);
        g.feed(b"\x1b[38;5mX"); // missing index
        let snap = g.snapshot();
        assert_eq!(snap.cells[0][0].ch, 'X');
    }

    #[test]
    fn sgr_unknown_param_is_ignored() {
        let mut g = PaneGrid::new(5, 1);
        g.feed(b"\x1b[999mX");
        let snap = g.snapshot();
        assert_eq!(snap.cells[0][0].ch, 'X');
        // Pen stays at default (no SGR 999 → fg unchanged).
        assert_eq!(snap.cells[0][0].fg, Color::WHITE);
    }

    #[test]
    fn sgr_empty_params_resets() {
        let mut g = PaneGrid::new(5, 1);
        g.feed(b"\x1b[31m"); // set red
        g.feed(b"\x1b[m"); // empty params = reset
        g.feed(b"X");
        let snap = g.snapshot();
        assert_eq!(snap.cells[0][0].fg, Color::WHITE);
    }

    #[test]
    fn sgr_bright_bg_100_107() {
        let mut g = PaneGrid::new(3, 1);
        g.feed(b"\x1b[104mX"); // bright blue background
        let snap = g.snapshot();
        let bright_blue = tear_types::pane_snapshot::ANSI_BRIGHT_COLORS[4];
        assert_eq!(snap.cells[0][0].bg, bright_blue);
    }

    #[test]
    fn sgr_disable_attrs_21_to_29() {
        let mut g = PaneGrid::new(3, 1);
        g.feed(b"\x1b[1;4;7m"); // bold + underline + inverse
        g.feed(b"\x1b[22;24;27m"); // disable each
        g.feed(b"X");
        let snap = g.snapshot();
        assert!(snap.cells[0][0].attrs.is_empty());
    }

    // ── Erase + edit edge cases ───────────────────────────────

    #[test]
    fn ech_past_end_of_row_clamps() {
        let mut g = PaneGrid::new(3, 1);
        g.feed(b"abc");
        g.feed(b"\x1b[1;2H"); // cursor to (0, 1)
        g.feed(b"\x1b[100X"); // erase 100 cells — clamps to row end
        let snap = g.snapshot();
        assert_eq!(snap.cells[0][0].ch, 'a');
        assert_eq!(snap.cells[0][1].ch, ' ');
        assert_eq!(snap.cells[0][2].ch, ' ');
    }

    #[test]
    fn ich_at_end_of_row_no_overflow() {
        let mut g = PaneGrid::new(3, 1);
        g.feed(b"abc");
        g.feed(b"\x1b[1;3H"); // cursor at last col
        g.feed(b"\x1b[5@"); // insert 5
        let snap = g.snapshot();
        // After insert: row truncated to 3 cells; the original 'c'
        // was at col 2 and gets pushed off.
        assert_eq!(snap.cells[0][0].ch, 'a');
        assert_eq!(snap.cells[0][1].ch, 'b');
        assert_eq!(snap.cells[0][2].ch, ' ');
    }

    #[test]
    fn dch_more_than_row_clamps() {
        let mut g = PaneGrid::new(3, 1);
        g.feed(b"abc");
        g.feed(b"\x1b[1;1H");
        g.feed(b"\x1b[100P"); // delete 100 cells
        let snap = g.snapshot();
        for c in 0..3 {
            assert_eq!(snap.cells[0][c].ch, ' ', "col {c}");
        }
    }

    // ── OSC + title edge cases ────────────────────────────────

    #[test]
    fn osc_with_no_params_is_dropped() {
        let mut g = PaneGrid::new(3, 1);
        g.feed(b"\x1b]\x07"); // empty OSC
        let snap = g.snapshot();
        assert!(snap.title.is_none());
    }

    #[test]
    fn osc_very_long_title_works() {
        let mut g = PaneGrid::new(3, 1);
        let long_title: String = "x".repeat(1000);
        let payload = format!("\x1b]2;{}\x07", long_title);
        g.feed(payload.as_bytes());
        assert_eq!(g.title().map(str::len), Some(1000));
    }

    // ── DEC mode interactions ─────────────────────────────────

    #[test]
    fn dec_1049_save_and_restore_cursor_around_alt_screen() {
        let mut g = PaneGrid::new(10, 3);
        g.feed(b"AAA\r\nBBB");
        // cursor at (1, 3)
        g.feed(b"\x1b[?1049h"); // enter alt + save cursor
        g.feed(b"\x1b[5;5H"); // move cursor in alt
        let alt = g.snapshot();
        assert!(alt.alt_screen_active);
        // Leave alt — cursor restored to (1, 3).
        g.feed(b"\x1b[?1049l");
        let back = g.snapshot();
        assert!(!back.alt_screen_active);
        assert_eq!(back.cursor_row, 1);
        assert_eq!(back.cursor_col, 3);
        // Primary preserved.
        assert_eq!(back.cells[0][0].ch, 'A');
        assert_eq!(back.cells[1][0].ch, 'B');
    }

    #[test]
    fn dec_25_cursor_visibility_round_trip() {
        let mut g = PaneGrid::new(3, 1);
        g.feed(b"\x1b[?25l"); // hide
        assert!(!g.snapshot().cursor_visible);
        g.feed(b"\x1b[?25h"); // show
        assert!(g.snapshot().cursor_visible);
        g.feed(b"\x1b[?25l"); // hide again
        assert!(!g.snapshot().cursor_visible);
    }

    // ── Misc robustness ───────────────────────────────────────

    #[test]
    fn bel_does_not_crash_or_consume_cell() {
        let mut g = PaneGrid::new(3, 1);
        g.feed(b"A\x07B"); // BEL between two chars
        let snap = g.snapshot();
        assert_eq!(snap.cells[0][0].ch, 'A');
        assert_eq!(snap.cells[0][1].ch, 'B');
    }

    #[test]
    fn tab_aligns_to_next_multiple_of_8() {
        let mut g = PaneGrid::new(20, 1);
        g.feed(b"\tX"); // tab from col 0 → col 8
        let snap = g.snapshot();
        assert_eq!(snap.cells[0][8].ch, 'X');
    }

    #[test]
    fn resize_to_zero_clamps_safely() {
        let mut g = PaneGrid::new(5, 3);
        g.feed(b"hello");
        // PaneGrid documents "max(1)" — but the constructor accepts
        // 0 cols/rows in theory. Resize to 0 should not panic.
        g.resize(0, 0);
        let snap = g.snapshot();
        // Cursor clamped to 0,0 since rows.saturating_sub(1) = 0.
        assert_eq!(snap.cursor_row, 0);
        assert_eq!(snap.cursor_col, 0);
    }

    #[test]
    fn ris_resets_pen_and_clears_screen() {
        let mut g = PaneGrid::new(5, 2);
        g.feed(b"\x1b[31m"); // red pen
        g.feed(b"AB\r\nCD");
        g.feed(b"\x1bc"); // RIS
        let snap = g.snapshot();
        for row in snap.cells {
            for cell in row {
                assert_eq!(cell.ch, ' ');
                assert_eq!(cell.fg, Color::WHITE);
            }
        }
        assert_eq!(snap.cursor_row, 0);
        assert_eq!(snap.cursor_col, 0);
    }
}

#[cfg(test)]
mod proptests {
    use super::*;
    use proptest::prelude::*;

    proptest! {
        /// No matter what bytes we feed (printable, control, malformed
        /// escapes, anything), PaneGrid never panics + the cursor stays
        /// inside the grid + the snapshot has the right dimensions.
        #[test]
        fn random_bytes_never_panic_and_cursor_stays_in_bounds(
            cols in 1usize..=80,
            rows in 1usize..=24,
            bytes in proptest::collection::vec(any::<u8>(), 0..2048),
        ) {
            let mut g = PaneGrid::new(cols, rows);
            g.feed(&bytes);
            let snap = g.snapshot();
            prop_assert_eq!(snap.cols, cols);
            prop_assert_eq!(snap.rows, rows);
            prop_assert_eq!(snap.cells.len(), rows);
            for row in &snap.cells {
                prop_assert_eq!(row.len(), cols);
            }
            prop_assert!(snap.cursor_row < rows.max(1));
            prop_assert!(snap.cursor_col < cols.max(1));
        }

        /// Plain ASCII printable runs advance the cursor by exactly
        /// `min(len, capacity)` cells, accounting for wrap.
        #[test]
        fn printable_ascii_runs_fill_cells_in_order(
            text in r"[A-Za-z0-9 ]{1,40}",
        ) {
            let mut g = PaneGrid::new(40, 3);
            g.feed(text.as_bytes());
            let snap = g.snapshot();
            for (i, c) in text.chars().enumerate() {
                if i < snap.cols {
                    prop_assert_eq!(snap.cells[0][i].ch, c);
                }
            }
        }

        /// Snapshot text dimensions always match snapshot.cols × rows.
        #[test]
        fn snapshot_text_dimensions_match(
            cols in 1usize..=120,
            rows in 1usize..=40,
            bytes in proptest::collection::vec(any::<u8>(), 0..1024),
        ) {
            let mut g = PaneGrid::new(cols, rows);
            g.feed(&bytes);
            let snap = g.snapshot();
            let text_rows = snap.to_text_rows();
            prop_assert_eq!(text_rows.len(), rows);
            for row in &text_rows {
                // chars().count() because some control codes (BEL etc.)
                // never reach print so the rows stay exactly cols wide.
                prop_assert_eq!(row.chars().count(), cols);
            }
        }

        /// Resize never panics + cursor is in bounds afterwards.
        #[test]
        fn resize_keeps_cursor_in_bounds(
            cols1 in 1usize..=60,
            rows1 in 1usize..=20,
            cols2 in 1usize..=60,
            rows2 in 1usize..=20,
            bytes in proptest::collection::vec(any::<u8>(), 0..512),
        ) {
            let mut g = PaneGrid::new(cols1, rows1);
            g.feed(&bytes);
            g.resize(cols2, rows2);
            let snap = g.snapshot();
            prop_assert_eq!(snap.cols, cols2);
            prop_assert_eq!(snap.rows, rows2);
            prop_assert!(snap.cursor_row < rows2.max(1));
            prop_assert!(snap.cursor_col < cols2.max(1));
        }
    }
}
