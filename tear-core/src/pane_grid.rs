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

use tear_types::pane_snapshot::{CellAttrs, Color, ansi_256_color, default_ansi_palette};
use tear_types::graphics::{Graphic, GraphicProtocol, GRAPHIC_PAYLOAD_MAX};
use tear_types::host_role::{HostRole, TearCaps};
use tear_types::modes::{
    AltScreen, AutoWrap, BracketedPaste, CursorKeys, CursorVisible, FocusReporting, ModeSet,
    MouseSgr, MouseTracking, SyncOutput,
};
use unicode_width::UnicodeWidthChar;
use vte::{Params, Parser, Perform};

pub use tear_types::pane_snapshot::{Cell, PaneSnapshot};

/// Maximum scrollback rows kept off-screen.
///
/// **Default: `usize::MAX` — unlimited.** The operator-facing
/// contract is "never lose anything"; the only ceiling is host
/// RAM. Consumers that want bounded retention (low-RAM systems,
/// log panes that emit billions of lines) override at construction
/// via [`PaneGrid::with_scrollback`].
///
/// Pre-2026-05 default was 1,000 rows (xterm tradition); changed to
/// match operator expectation of "I can always scroll back to
/// anything I've seen in this pane." See
/// `tear-config/src/lib.rs::ScrollbackConfig` for the operator-
/// facing tunable surface and the documented opt-in to bounded mode.
pub const DEFAULT_SCROLLBACK_ROWS: usize = usize::MAX;

/// Live grid + cursor + the parser that feeds them. Owns mutable
/// state, so callers wrap it in `Mutex` (the `InProcess` does this
/// since multiple PTY-reader threads + the RPC dispatch thread all
/// race for it).
pub struct PaneGrid {
    parser: Parser,
    pub(crate) state: GridState,
    /// APC re-assembly, because vte cannot do it for us.
    ///
    /// vte 0.15's `Perform` has `hook`/`put`/`unhook` for DCS but **no APC
    /// method at all**: on `ESC _` it enters `State::SosPmApcString` and
    /// consumes every byte to the terminator with no callback. So the
    /// kitty graphics protocol — which is APC-framed — was invisible to
    /// this parser, and an image vanished with no error and no flag.
    ///
    /// The fix is to lift APC out of the stream BEFORE vte sees it. That
    /// is what mado does too; this is the same interception, moved to the
    /// authority.
    apc: ApcScanner,
}

/// Splits `ESC _ … ESC \` (or `BEL`) out of a byte stream.
///
/// A payload can be megabytes and arrives over many PTY reads, so the scan
/// is a resumable state machine rather than a search over one buffer — an
/// APC split across `feed()` calls must reassemble, which is exactly the
/// chunk-boundary case the espelho conformance rows already pin for
/// ordinary escapes.
#[derive(Debug, Default)]
struct ApcScanner {
    state: ApcState,
    buf: Vec<u8>,
    /// Set the moment `buf` hits the cap, so the fact survives the params
    /// being stripped later.
    cut: bool,
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
enum ApcState {
    /// Not in an APC, and no `ESC` pending.
    #[default]
    Idle,
    /// Saw `ESC`; the next byte decides whether this is an APC.
    Escape,
    /// Inside an APC payload.
    Inside,
    /// Inside an APC and saw `ESC`; `\` terminates (ST).
    InsideEscape,
}

impl ApcScanner {
    /// Feed `bytes`, returning the stream with APC sequences removed plus
    /// any payloads that completed, each with whether it was CUT.
    ///
    /// The cut flag is CARRIED rather than re-derived downstream. It was
    /// briefly re-derived by comparing the final payload length against the
    /// cap, which is wrong for a reason worth keeping: the cap applies to
    /// the whole APC body, and kitty's params (`Ga=T,f=100;`) are stripped
    /// before storage — so a truncated payload came back a few bytes UNDER
    /// the cap and reported itself intact. A fact known at the boundary
    /// must not be reconstructed from a proxy after the shape changes.
    ///
    /// A lone `ESC` at the end of a chunk is HELD, not emitted — emitting
    /// it would hand vte a truncated escape and the following chunk's
    /// bytes would be misparsed as its parameters.
    fn split(&mut self, bytes: &[u8]) -> (Vec<u8>, Vec<(Vec<u8>, bool)>) {
        let mut passthrough = Vec::with_capacity(bytes.len());
        let mut done = Vec::new();
        for &b in bytes {
            match self.state {
                ApcState::Idle => {
                    if b == 0x1b {
                        self.state = ApcState::Escape;
                    } else {
                        passthrough.push(b);
                    }
                }
                ApcState::Escape => {
                    if b == b'_' {
                        // An APC opens: the ESC we withheld belongs to it.
                        self.state = ApcState::Inside;
                        self.buf.clear();
                        self.cut = false;
                    } else {
                        // Not an APC — replay the withheld ESC, then
                        // re-handle this byte (it may itself be an ESC,
                        // e.g. `ESC ESC`).
                        passthrough.push(0x1b);
                        if b == 0x1b {
                            self.state = ApcState::Escape;
                        } else {
                            passthrough.push(b);
                            self.state = ApcState::Idle;
                        }
                    }
                }
                ApcState::Inside => match b {
                    0x1b => self.state = ApcState::InsideEscape,
                    // BEL terminates too — xterm accepts it for APC/OSC.
                    0x07 => {
                        done.push((std::mem::take(&mut self.buf), self.cut));
                        self.state = ApcState::Idle;
                    }
                    _ => {
                        if self.buf.len() < GRAPHIC_PAYLOAD_MAX {
                            self.buf.push(b);
                        } else {
                            self.cut = true;
                        }
                    }
                },
                ApcState::InsideEscape => {
                    if b == b'\\' {
                        done.push((std::mem::take(&mut self.buf), self.cut));
                        self.state = ApcState::Idle;
                    } else {
                        // An ESC inside the payload that was not ST.
                        if self.buf.len() < GRAPHIC_PAYLOAD_MAX {
                            self.buf.push(0x1b);
                            self.buf.push(b);
                        } else {
                            self.cut = true;
                        }
                        self.state = ApcState::Inside;
                    }
                }
            }
        }
        (passthrough, done)
    }
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
    /// DECCKM cursor-keys application mode (DEC mode 1 — CSI ? 1 h/l).
    /// When set, host keystrokes for Up/Down/Right/Left should be
    /// encoded as `ESC O A/B/C/D` instead of `ESC [ A/B/C/D`. Reset
    /// on RIS (ESC c) and DECSTR (CSI ! p).
    cursor_keys_mode: bool,
    /// Last printed char — REP (CSI b) repeats this.
    last_printed: Option<char>,
    /// Who answers VT queries on this pane. `Relay` (the default) means
    /// tear answers nothing and the attached terminal is the host — the
    /// behaviour tear has always had.
    role: HostRole,
    /// DEC 7 (DECAWM) — autowrap. On by default, per xterm.
    autowrap: bool,
    /// DEC 1004 — focus in/out reporting.
    focus_reporting: bool,
    /// DEC 2004 — bracketed paste. Gates paste sanitisation downstream.
    bracketed_paste: bool,
    /// DEC 2026 — synchronized output.
    sync_output: bool,
    /// DEC 1000/1002/1003 — mouse tracking level (mutually exclusive).
    mouse: MouseTracking,
    /// DEC 1006 — SGR extended mouse encoding.
    mouse_sgr: bool,
    /// Combining-mark table — see [`PaneSnapshot::combining`]. Cells hold a
    /// 1-based index into this; `0` means no marks.
    combining: Vec<Vec<char>>,
    /// Images transmitted into this pane, undecoded. See
    /// [`tear_types::graphics`] for why the authority stores bytes rather
    /// than pixels.
    graphics: Vec<Graphic>,
    /// Payload being accumulated by an in-flight DCS sixel sequence
    /// (`hook` → `put`* → `unhook`). `None` when no DCS is open.
    sixel_in_flight: Option<Vec<u8>>,
    /// Reply bytes owed to the child process, drained by the runtime and
    /// written back to the PTY.
    ///
    /// Always empty while `role` is `Relay`, which is what makes the
    /// response path a no-op until the shuken flip deliberately turns it
    /// on. A reply is data the CHILD asked for, so it goes to the PTY's
    /// input side, never into the grid.
    pending_response: Vec<u8>,
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
            // Allocate a modest initial capacity even when
            // scrollback_cap is unlimited (usize::MAX). VecDeque
            // grows on push, so the initial size is just an
            // amortisation hint; allocating usize::MAX directly
            // would OOM the host. 64 rows is a fine warm-up
            // budget — the deque doubles from there on demand.
            scrollback: VecDeque::with_capacity(64.min(scrollback_cap)),
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
            cursor_keys_mode: false,
            last_printed: None,
            role: HostRole::default(),
            // Autowrap is ON by default (xterm); everything else is off.
            autowrap: true,
            focus_reporting: false,
            bracketed_paste: false,
            sync_output: false,
            mouse: MouseTracking::Off,
            mouse_sgr: false,
            combining: Vec::new(),
            graphics: Vec::new(),
            sixel_in_flight: None,
            pending_response: Vec::new(),
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

    /// Consume one complete APC payload (the bytes between `ESC _` and its
    /// terminator, exclusive).
    ///
    /// Only the kitty graphics protocol is recognised — its payloads start
    /// with `G`. Any other APC is dropped, which matches every terminal:
    /// APC is a private-use channel and an unrecognised one carries no
    /// meaning we could act on.
    fn ingest_apc(&mut self, payload: &[u8], cut: bool) {
        let Some((&b'G', rest)) = payload.split_first() else {
            return;
        };
        // Kitty's framing is `G<key=value,...>;<base64 payload>`. The
        // params are ASCII and the payload is not, so split on the FIRST
        // `;` and never parse past it — a control key that happens to
        // appear inside base64 must not be read as one.
        let (params, data) = match rest.iter().position(|&b| b == b';') {
            Some(i) => (&rest[..i], &rest[i + 1..]),
            // No `;` at all: a control-only command (query, delete). Real,
            // and it carries no image.
            None => (rest, &[][..]),
        };
        self.push_graphic(
            GraphicProtocol::Kitty,
            String::from_utf8_lossy(params).into_owned(),
            data.to_vec(),
            cut,
        );
    }

    /// Record a transmitted image at the current cursor position.
    ///
    /// The single place a graphic enters the grid, so the payload bound and
    /// the truncation flag cannot be applied inconsistently by protocol.
    fn push_graphic(
        &mut self,
        protocol: GraphicProtocol,
        params: String,
        mut data: Vec<u8>,
        cut_upstream: bool,
    ) {
        // `cut_upstream` is the boundary's own verdict; the length check is
        // only for producers that hand over an unbounded buffer (the DCS
        // path bounds as it accumulates, so both agree there). Never rely on
        // the length alone — see `ApcScanner::split`.
        let truncated = cut_upstream || data.len() > GRAPHIC_PAYLOAD_MAX;
        if data.len() > GRAPHIC_PAYLOAD_MAX {
            data.truncate(GRAPHIC_PAYLOAD_MAX);
        }
        self.graphics.push(Graphic {
            protocol,
            params,
            data,
            at_row: self.cursor_row,
            at_col: self.cursor_col,
            truncated,
        });
    }

    /// Queue a reply to the child, if and only if this pane is the host.
    ///
    /// The role check lives HERE, at the single chokepoint, rather than at
    /// each call site. Every query arm calls `answer` unconditionally, so a
    /// newly-added query cannot forget the check and start replying while
    /// tear is still a relay — which would mean two answers on the wire.
    fn answer(&mut self, bytes: &[u8]) {
        if self.role.answers_queries() {
            self.pending_response.extend_from_slice(bytes);
        }
    }

    /// Read one cell on whichever screen is active.
    fn active_cell_at(&self, row: usize, col: usize) -> Option<&Cell> {
        if self.alt_active {
            self.alternate.get(row).and_then(|r| r.get(col))
        } else {
            self.primary.get(row).and_then(|r| r.get(col))
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
            width: 1,
            combining: 0,
        }
    }

    /// The cell for a printed glyph. `w` is its display width: `1` normal,
    /// `2` the lead of a double-width glyph.
    fn current_cell_for_print(&self, ch: char, w: u8) -> Cell {
        Cell {
            ch,
            fg: self.pen_fg,
            bg: self.pen_bg,
            attrs: self.pen_attrs,
            width: w,
            // A freshly printed glyph carries no marks; a following
            // zero-width codepoint attaches them.
            combining: 0,
        }
    }

    /// The continuation half of a double-width glyph.
    ///
    /// It carries the LEAD's pen colours, not the default pen: a
    /// default-styled spacer under a coloured lead renders as a visible seam
    /// through the middle of the glyph.
    fn continuation_cell(&self) -> Cell {
        Cell {
            ch: ' ',
            fg: self.pen_fg,
            bg: self.pen_bg,
            attrs: self.pen_attrs,
            width: 0,
            combining: 0,
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

    /// Advance past a glyph of display width `w`.
    ///
    /// `w` is the glyph's WIDTH, not `1`. That distinction is the whole
    /// wide-character axis: advancing by one after a double-width glyph puts
    /// every later cell on the row one column left of where the child process
    /// believes it is.
    fn advance_cursor_after_print(&mut self, w: usize) {
        let adv = w.max(1);
        if self.cursor_col + adv >= self.cols {
            self.park_at_right_margin();
        } else {
            self.cursor_col += adv;
        }
    }

    /// Park the cursor on the LAST column and arm the deferred wrap.
    ///
    /// The clamp is load-bearing and was previously invisible: with a
    /// 1-column advance the flag could only be raised when the cursor was
    /// already at `cols - 1`, so clamping was a no-op. At width 2 it is not —
    /// a glyph landing flush against the margin would otherwise leave the
    /// cursor on its own LEAD, one column left of the truth, which shows up
    /// as every subsequent relative motion being off by one and `CSI 6n`
    /// under-reporting the column.
    fn park_at_right_margin(&mut self) {
        self.cursor_col = self.cols.saturating_sub(1);
        self.wrap_pending = true;
    }

    /// Blank any half-glyph this write is about to orphan.
    ///
    /// Fills with [`Cell::BLANK`] and NOT `blank_cell()`: the pen-background
    /// blank would paint the current background into a cell the glyph never
    /// owned, which diverges from mado on any coloured background.
    fn clear_orphans_at(&mut self, row: usize, col: usize, w: usize) {
        // Left edge — we are overwriting a continuation, so its lead (one to
        // the left) loses its other half and must go.
        if col > 0 && self.active_cell_at(row, col).is_some_and(Cell::is_continuation) {
            if let Some(lead) = self.active_cell_mut(row, col - 1) {
                *lead = Cell::BLANK;
            }
        }
        // Right edge — the last column we occupy holds a wide LEAD, so its
        // continuation to the right is about to be orphaned.
        let last = col + w.saturating_sub(1);
        if self.active_cell_at(row, last).is_some_and(|c| c.width == 2) && last + 1 < self.cols {
            if let Some(cont) = self.active_cell_mut(row, last + 1) {
                *cont = Cell::BLANK;
            }
        }
    }

    /// Attach a zero-width codepoint to the glyph that precedes the cursor.
    ///
    /// Three behaviours copied deliberately from mado, each of which a
    /// "reasonable" implementation gets wrong:
    ///
    /// 1. **Does not set `last_printed`**, so `CSI b` (REP) repeats the
    ///    BASE glyph rather than the mark.
    /// 2. **Does not clear `wrap_pending`**, so a deferred wrap stays armed
    ///    across a mark.
    /// 3. **Walks back to the LEAD column.** When `wrap_pending` is set the
    ///    search starts at `cols - 1`, and that cell may be the
    ///    CONTINUATION of a margin-flush wide glyph whose lead is one to
    ///    its left. Taking `cols - 1` verbatim attaches the mark to a
    ///    width-0 cell, where it renders nowhere — the regression mado
    ///    fixed on 2026-07-30. tear is born with the fix.
    ///
    /// A mark with no base cell (the first codepoint of a line) is dropped,
    /// matching mado.
    fn combine_into_previous(&mut self, c: char) {
        let start = if self.wrap_pending {
            self.cols.saturating_sub(1)
        } else if self.cursor_col > 0 {
            self.cursor_col - 1
        } else {
            return;
        };
        let row = self.cursor_row;
        let col = self.lead_col_at(row, start);
        if col >= self.cols || row >= self.rows {
            return;
        }
        // Resolve the cell's existing table slot before taking the &mut, so
        // the table borrow and the cell borrow never overlap.
        let existing = self
            .active_cell_at(row, col)
            .map_or(0, |cell| cell.combining);
        if existing == 0 {
            // u16 is the index width; refuse to mint past it rather than
            // wrapping into another cell's marks.
            let Ok(next) = u16::try_from(self.combining.len() + 1) else {
                return;
            };
            self.combining.push(vec![c]);
            if let Some(cell) = self.active_cell_mut(row, col) {
                cell.combining = next;
            } else {
                // The cell vanished between the read and the write — drop
                // the entry rather than leaving it orphaned.
                self.combining.pop();
            }
        } else if let Some(marks) = self.combining.get_mut(existing as usize - 1) {
            marks.push(c);
        }
    }

    /// Walk left to the lead column of whatever glyph owns `col`.
    ///
    /// If `col` holds a continuation cell the glyph's lead is at `col - 1`;
    /// otherwise `col` is already the lead.
    fn lead_col_at(&self, row: usize, col: usize) -> usize {
        if col > 0 && self.active_cell_at(row, col).is_some_and(Cell::is_continuation) {
            col - 1
        } else {
            col
        }
    }

    /// Place one glyph of display width `w` at the cursor and advance.
    fn put_char(&mut self, c: char, w: usize) {
        // Honour a deferred wrap from the previous print, then place.
        if self.wrap_pending {
            self.wrap_pending = false;
            self.cursor_col = 0;
            self.linefeed();
        }
        // A double-width glyph that cannot fit before the right margin wraps
        // WHOLE. Splitting it across the seam would put half a glyph in each
        // row, which no renderer can draw correctly.
        if w == 2 && self.cursor_col + 1 >= self.cols {
            self.cursor_col = 0;
            self.linefeed();
        }
        let row = self.cursor_row;
        let col = self.cursor_col;
        let cell = self.current_cell_for_print(c, w as u8);

        if self.insert_mode {
            // IRM shifts by the glyph's WIDTH, not by one column.
            let cols = self.cols;
            let cont = self.continuation_cell();
            if let Some(r) = self.active_row_mut(row) {
                if col < r.len() {
                    r.insert(col, cell);
                    if w == 2 && col + 1 <= r.len() {
                        r.insert(col + 1, cont);
                    }
                    r.truncate(cols);
                }
            }
        } else {
            self.clear_orphans_at(row, col, w);
            if let Some(slot) = self.active_cell_mut(row, col) {
                *slot = cell;
            }
            if w == 2 && col + 1 < self.cols {
                let cont = self.continuation_cell();
                if let Some(slot) = self.active_cell_mut(row, col + 1) {
                    *slot = cont;
                }
            }
        }
        self.last_printed = Some(c);
        self.advance_cursor_after_print(w);
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
        // ── PARAMETERS AND SUB-PARAMETERS ARE NOT THE SAME THING ──────
        //
        // SGR has two spellings for an extended colour, and they are NOT
        // interchangeable:
        //
        //   ESC[38;2;r;g;b m      five PARAMETERS      (legacy xterm)
        //   ESC[38:2:cs:r:g:b m   one parameter with
        //                         six SUB-PARAMETERS   (ISO 8613-6)
        //
        // The colon form carries a colour-space id in slot 2, which is
        // almost always empty (`38:2::r:g:b`) and arrives as a 0.
        //
        // This used to `flat_map` both spellings into one stream, which
        // erases the distinction. The colon form then read the empty
        // colour-space slot AS THE RED CHANNEL: every channel shifted by
        // one and the real blue component fell out the end of the colour
        // and was executed as an SGR attribute code. Measured live in a
        // tear pane before this change:
        //
        //   ESC[38;2;248;248;242m  -> fg (248,248,242)   correct
        //   ESC[38:2::248:248:242m -> fg (0,248,248)     WRONG
        //
        // and when that trailing component happened to be 4, UNDERLINE
        // latched on for the rest of the session. SGR 58/59 (underline
        // colour) had the same shape of bug from the other direction: 58
        // was dropped as unknown and its components then walked as
        // attribute codes, so `ESC[58;5;4m` also stuck UNDERLINE on.
        //
        // So: walk PARAMETERS, and let a parameter that carries
        // sub-parameters be self-contained.
        let items: Vec<&[u16]> = params.iter().collect();
        if items.is_empty() {
            self.sgr_reset();
            return;
        }
        let mut idx = 0;
        while idx < items.len() {
            let param = items[idx];
            let Some(&code) = param.first() else {
                idx += 1;
                continue;
            };

            // Colon form: everything this directive needs is in `param`.
            if param.len() > 1 {
                self.apply_sgr_subparams(param);
                idx += 1;
                continue;
            }

            // Semicolon form: 38/48/58 consume the parameters that follow.
            // 58/59 are underline COLOUR — tear's CellAttrs has no
            // underline-colour field, so the value is discarded, but the
            // parameters must still be CONSUMED or they walk as codes.
            if matches!(code, 38 | 48 | 58) {
                let (colour, consumed) = self.parse_extended_color_params(&items[idx..]);
                match (code, colour) {
                    (38, Some(c)) => self.pen_fg = c,
                    (48, Some(c)) => self.pen_bg = c,
                    _ => {}
                }
                idx += consumed;
                continue;
            }

            self.apply_sgr_code(code);
            idx += 1;
        }
    }

    /// One SGR directive spelled with sub-parameters (`38:2::r:g:b`,
    /// `38:5:n`, `4:3`, `58:2::r:g:b`). Self-contained by construction.
    fn apply_sgr_subparams(&mut self, param: &[u16]) {
        match param[0] {
            // Styled underline. tear's CellAttrs carries a single boolean,
            // so every style except `4:0` is "on"; `4:0` is the modern
            // spelling of SGR 24.
            4 => {
                if param[1] == 0 {
                    self.pen_attrs.remove(CellAttrs::UNDERLINE);
                } else {
                    self.pen_attrs.insert(CellAttrs::UNDERLINE);
                }
            }
            code @ (38 | 48 | 58) => {
                let colour = match param[1] {
                    5 => param.get(2).map(|&n| ansi_256_color(n, &self.palette)),
                    // `38:2:cs:r:g:b` has SIX slots — skip the colour-space
                    // id. `38:2:r:g:b` (five) omits it. Choosing by length
                    // is what keeps the channels aligned.
                    2 => match param.len() {
                        n if n >= 6 => {
                            Some(Color::new(param[3] as u8, param[4] as u8, param[5] as u8))
                        }
                        5 => Some(Color::new(param[2] as u8, param[3] as u8, param[4] as u8)),
                        _ => None,
                    },
                    _ => None,
                };
                match (code, colour) {
                    (38, Some(c)) => self.pen_fg = c,
                    (48, Some(c)) => self.pen_bg = c,
                    // 58 = underline colour: parsed so it cannot leak,
                    // then dropped because there is nowhere to put it.
                    _ => {}
                }
            }
            other => self.apply_sgr_code(other),
        }
    }

    /// Semicolon-form extended colour. `rest[0]` is the 38/48/58
    /// directive. Returns the colour (None for 58 or malformed input) and
    /// how many PARAMETERS were consumed — always at least 1, so the
    /// caller can never fail to advance.
    fn parse_extended_color_params(&self, rest: &[&[u16]]) -> (Option<Color>, usize) {
        let first = |i: usize| rest.get(i).and_then(|p| p.first().copied());
        match first(1) {
            Some(5) => match first(2) {
                // `self.palette`, not the default one: a pane whose
                // palette has been re-set by OSC 4 must resolve indexed
                // colours against ITS palette.
                Some(n) => (Some(ansi_256_color(n, &self.palette)), 3),
                None => (None, 2),
            },
            Some(2) => match (first(2), first(3), first(4)) {
                (Some(r), Some(g), Some(b)) => (Some(Color::new(r as u8, g as u8, b as u8)), 5),
                _ => (None, rest.len().min(5)),
            },
            _ => (None, 1),
        }
    }

    fn apply_sgr_code(&mut self, code: u16) {
        {
            let p = code;
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
                39 => self.pen_fg = Color::WHITE,
                40..=47 => self.pen_bg = self.palette[(p - 40) as usize],
                49 => self.pen_bg = Color::BLACK,
                90..=97 => self.pen_fg = self.palette[8 + (p - 90) as usize],
                100..=107 => self.pen_bg = self.palette[8 + (p - 100) as usize],
                _ => {} // unknown — drop
            }
        }
    }

    fn sgr_reset(&mut self) {
        self.pen_fg = Color::WHITE;
        self.pen_bg = Color::BLACK;
        self.pen_attrs = CellAttrs::NONE;
    }
}

impl Perform for GridState {
    fn print(&mut self, c: char) {
        // Pane-as-block: feed the extractor BEFORE placement so
        // its phase state reflects the same chronology the
        // grid sees. Cheap when the extractor is Idle (single
        // Option-is-none check).
        self.blocks.on_print(c);
        let w = UnicodeWidthChar::width(c).unwrap_or(1);
        if w == 0 {
            // A zero-width codepoint (a combining mark, a ZWJ) belongs to
            // the glyph before it and consumes NO column. Placing it in a
            // cell of its own — which this parser used to do — displaces
            // every later cell on the row.
            self.combine_into_previous(c);
            return;
        }
        self.put_char(c, w);
    }

    /// DCS opened. `q` is sixel; everything else is ignored as before.
    ///
    /// Accumulation starts here and the payload is bounded as it grows, not
    /// at `unhook` — a hostile stream that never terminates would otherwise
    /// grow the buffer without limit while the sequence stayed open.
    fn hook(&mut self, _params: &Params, _intermediates: &[u8], _ignore: bool, action: char) {
        if action == 'q' {
            self.sixel_in_flight = Some(Vec::new());
        }
    }

    fn put(&mut self, byte: u8) {
        if let Some(buf) = self.sixel_in_flight.as_mut() {
            // One past the cap is enough to know it was cut; keeping more
            // would defeat the bound.
            if buf.len() < GRAPHIC_PAYLOAD_MAX {
                buf.push(byte);
            }
        }
    }

    fn unhook(&mut self) {
        if let Some(data) = self.sixel_in_flight.take() {
            if !data.is_empty() {
                // The DCS path bounds as it accumulates, so a payload at
                // the cap is exactly the cut case.
                let cut = data.len() >= GRAPHIC_PAYLOAD_MAX;
                self.push_graphic(GraphicProtocol::Sixel, String::new(), data, cut);
            }
        }
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

    fn csi_dispatch(&mut self, params: &Params, intermediates: &[u8], _ignore: bool, c: char) {
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
        // ── Private-parameter CSI is a SEPARATE NAMESPACE ─────────────
        //
        // A prefix byte in 0x3C..=0x3F (`<` `=` `>` `?`) makes the
        // sequence private: it shares FINAL BYTES with the standard
        // sequences but means something entirely different. Dispatching
        // on the final byte alone therefore runs the wrong command.
        //
        // Not hypothetical. Claude Code emits `CSI > 4 ; 2 m` (xterm
        // XTMODKEYS / modifyOtherKeys) at startup. Read as SGR that is
        // params [4, 2] → UNDERLINE + DIM latched onto the pen, and
        // since nothing later emits SGR 0 or 24, every cell printed for
        // the rest of the session came out underlined — the standing
        // "everything is underlined in mado" artifact. Measured on a
        // real capture: 205/205 non-blank cells underlined before this
        // guard, 0 after. `CSI > … h` and `CSI ? … J/K` (DECSED/DECSEL)
        // are the same class one final byte over.
        //
        // So: recognise the private sequences we implement, and make
        // every other private sequence a no-op. Falling through to the
        // standard `match` is what must stay unrepresentable — adding a
        // new standard arm must never silently hand some private
        // sequence a meaning it does not have.
        if let Some(prefix) = intermediates
            .first()
            .copied()
            .filter(|b| (0x3C..=0x3F).contains(b))
        {
            if prefix == b'?' && (c == 'h' || c == 'l') {
                let set = c == 'h';
                for p in params.iter() {
                    if let Some(&code) = p.first() {
                        self.apply_dec_mode(code, set);
                    }
                }
            }
            // Secondary DA (`CSI > c`). It lives HERE and not in the
            // standard match below precisely because of this namespace
            // split: `CSI c` and `CSI > c` share a final byte and are
            // different queries.
            if prefix == b'>' && c == 'c' {
                self.answer(TearCaps::SECONDARY_DA);
            }
            return;
        }
        match c {
            // ── VT queries — answered ONLY as HostRole::Host ──────────
            // DSR (CSI n): 5 = "are you ok", 6 = cursor position (CPR).
            'n' => match first {
                5 => self.answer(TearCaps::STATUS_OK),
                6 => {
                    // CPR is 1-based, and it reports the cursor's CLAMPED
                    // column — which is why park_at_right_margin matters:
                    // a cursor parked on a wide glyph's lead instead of the
                    // last column under-reports here.
                    let row = self.cursor_row + 1;
                    let col = self.cursor_col + 1;
                    let mut r = Vec::new();
                    r.extend_from_slice(b"\x1b[");
                    r.extend_from_slice(row.to_string().as_bytes());
                    r.push(b';');
                    r.extend_from_slice(col.to_string().as_bytes());
                    r.push(b'R');
                    self.answer(&r);
                }
                _ => {}
            },
            // Primary DA (CSI c / CSI 0 c).
            'c' => self.answer(TearCaps::PRIMARY_DA),
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
                            self.alternate.insert(self.scroll_top, blank);
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
        // OSC 7 — current working directory notification.
        // Most shells (zsh-vcs-info, bash with __vsc_*, ghostty's
        // shell-integration) emit `OSC 7 ; file://<host>/<path>`.
        // The block extractor stamps this onto every subsequent
        // block at prompt start.
        if matches!(code, Some("7"))
            && let Some(payload) = params.get(1).and_then(|p| std::str::from_utf8(p).ok())
        {
            self.blocks.set_cwd_from_osc7(payload);
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
                self.cursor_keys_mode = false;
                self.cursor_visible = true;
                self.title = None;
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
            1 => self.cursor_keys_mode = set,     // DECCKM
            25 => self.cursor_visible = set,      // DECTCEM
            7 => self.autowrap = set,             // DECAWM
            1004 => self.focus_reporting = set,   // focus in/out reporting
            2004 => self.bracketed_paste = set,   // bracketed paste
            2026 => self.sync_output = set,       // synchronized output
            // Mouse tracking levels are mutually exclusive: the LAST one
            // set wins, and resetting any of them turns tracking off. A
            // set of independent bools would let two levels be true at
            // once, which no terminal can mean.
            1000 => self.mouse = if set { MouseTracking::Click } else { MouseTracking::Off },
            1002 => self.mouse = if set { MouseTracking::Drag } else { MouseTracking::Off },
            1003 => self.mouse = if set { MouseTracking::Motion } else { MouseTracking::Off },
            1006 => self.mouse_sgr = set, // SGR extended mouse encoding
            _ => {}
        }
    }
}

impl PaneGrid {
    // ── THE AUTHORITY SEAL ──────────────────────────────────────────
    //
    // `new` / `with_scrollback` / `feed` are `pub(crate)`, and that
    // visibility IS the seal described in `docs/SHUKEN.md`.
    //
    // The decision there is that `PaneGrid` is the SOLE authoritative VT
    // parser for a pane. A doc cannot enforce that; a consumer that can
    // construct its own grid and feed it bytes has a second authority, and
    // the two can then disagree — which is the exact defect (mado's
    // `terminal.rs` double-parse) the decision exists to remove.
    //
    // SHUKEN originally proposed sealing this by removing `vte` from mado's
    // manifest. That is necessary and NOT sufficient: mado depends on
    // `tear-core` directly for `InProcess`, so with `vte` gone
    // `tear_core::PaneGrid::new(80, 24).feed(bytes)` still compiled — a
    // second authoritative grid that never names `vte` at all.
    //
    // `pub(crate)` closes it at the strongest available tier: outside this
    // crate the constructor is not merely discouraged, it is **E0603, a
    // private item**. A Cargo feature was considered and rejected — mado
    // NEEDS `InProcess` (which owns the PTYs and drives these grids
    // internally), so a feature that excluded `pane_grid` from mado would
    // break the very runtime the decision depends on, while a feature that
    // included it would seal nothing.
    //
    // Reading stays public on purpose: `snapshot()` and the `PaneSnapshot`
    // / `Cell` types below are how a client observes the authority. The
    // asymmetry is the whole design — **anyone may read, only the authority
    // may advance.**
    #[must_use]
    pub(crate) fn new(cols: usize, rows: usize) -> Self {
        Self::with_scrollback(cols, rows, DEFAULT_SCROLLBACK_ROWS)
    }

    #[must_use]
    pub(crate) fn with_scrollback(cols: usize, rows: usize, scrollback_cap: usize) -> Self {
        Self {
            parser: Parser::new(),
            state: GridState::new(cols, rows, scrollback_cap),
            apc: ApcScanner::default(),
        }
    }

    /// Advance this pane's terminal state by `bytes`.
    ///
    /// `pub(crate)` — see the authority-seal note above. This is the only
    /// write verb on a pane's grid, and it is reachable only from inside
    /// `tear-core`, i.e. only through `InProcess`/the daemon.
    pub(crate) fn feed(&mut self, bytes: &[u8]) {
        // Lift APC out first — vte would swallow it silently (see
        // `ApcScanner`). Everything else reaches the parser untouched.
        let (passthrough, apcs) = self.apc.split(bytes);
        self.parser.advance(&mut self.state, &passthrough);
        for (payload, cut) in apcs {
            self.state.ingest_apc(&payload, cut);
        }
    }

    /// Every terminal mode this pane is in, taken at ONE instant.
    ///
    /// This is how a client reads a mode under `docs/SHUKEN.md` — from the
    /// authority, never from a parser of its own. Today mado reads
    /// `bracketed_paste` from its OWN `Terminal`, which is correct only
    /// because mado still parses every byte; it becomes a live bug the
    /// instant this grid is authoritative, and it is a paste-sanitisation
    /// decision, not a cosmetic one.
    ///
    /// Returned as a whole `ModeSet` rather than one getter per mode so a
    /// client cannot mix modes from two different instants.
    #[must_use]
    pub fn modes(&self) -> ModeSet {
        let s = &self.state;
        ModeSet {
            bracketed_paste: BracketedPaste::new(s.bracketed_paste),
            cursor_keys: CursorKeys::new(s.cursor_keys_mode),
            focus_reporting: FocusReporting::new(s.focus_reporting),
            sync_output: SyncOutput::new(s.sync_output),
            mouse: s.mouse,
            mouse_sgr: MouseSgr::new(s.mouse_sgr),
            cursor_visible: CursorVisible::new(s.cursor_visible),
            autowrap: AutoWrap::new(s.autowrap),
            alt_screen: AltScreen::new(s.alt_active),
        }
    }

    /// Set who answers VT queries for this pane.
    ///
    /// See [`HostRole`]. Setting [`HostRole::Host`] while a client with its
    /// own parser is still attached means BOTH answer, and the second reply
    /// lands on the PTY as if the operator had typed it.
    pub(crate) fn set_host_role(&mut self, role: HostRole) {
        self.state.role = role;
    }

    /// Take the reply bytes owed to the child process.
    ///
    /// The caller writes these to the PTY's INPUT side — a reply is data
    /// the child asked for, not output to be rendered. Always empty while
    /// the pane is a [`HostRole::Relay`].
    #[must_use]
    pub(crate) fn take_response(&mut self) -> Option<Vec<u8>> {
        if self.state.pending_response.is_empty() {
            None
        } else {
            Some(std::mem::take(&mut self.state.pending_response))
        }
    }

    #[must_use]
    pub fn snapshot(&self) -> PaneSnapshot {
        let cells: Vec<Vec<Cell>> = self.state.active_rows().cloned().collect();
        // Carry the rolled-off scrollback so a re-attach / session switch
        // restores the pane's history (the primary screen only — the
        // alternate screen's apps own the full viewport and have no
        // scrollback to restore).
        let scrollback: Vec<Vec<Cell>> = if self.state.alt_active {
            Vec::new()
        } else {
            self.state.scrollback.iter().cloned().collect()
        };
        PaneSnapshot {
            rows: self.state.rows,
            cols: self.state.cols,
            cells,
            cursor_row: self.state.cursor_row,
            cursor_col: self.state.cursor_col,
            alt_screen_active: self.state.alt_active,
            cursor_visible: self.state.cursor_visible,
            title: self.state.title.clone(),
            cursor_keys_mode: self.state.cursor_keys_mode,
            scrollback,
            combining: self.state.combining.clone(),
            modes: self.modes(),
            graphics: self.state.graphics.clone(),
        }
    }

    /// Current window title (OSC 0 / 2). None until the first
    /// title set; cleared on RIS.
    #[must_use]
    pub fn title(&self) -> Option<&str> {
        self.state.title.as_deref()
    }

    /// Stamp the owning pane's provenance so every block this grid
    /// mints records WHO ran it. **Write-once**; returns `true` if
    /// this call took effect.
    ///
    /// Provenance enters through the grid deliberately. Under
    /// shuken (`docs/SHUKEN.md`) `PaneGrid` is the sole VT
    /// authority — it is the one place that sees the byte stream
    /// and mints blocks from it — so attribution belongs at the
    /// same seam as the authority. A second, parallel path that
    /// attributed blocks anywhere else would be exactly the
    /// duplicated-state split shuken exists to forbid.
    pub fn stamp_yurai(&mut self, y: tear_types::Yurai) -> bool {
        self.state.blocks.stamp_yurai(y)
    }

    /// Provenance every block from this grid carries.
    #[must_use]
    pub fn yurai(&self) -> &tear_types::Yurai {
        self.state.blocks.yurai()
    }

    /// DECCKM (DEC mode 1) cursor-keys application mode.
    ///
    /// Consumers translating host keystrokes to PTY bytes (mado's
    /// `keybind::madori_key_to_pty_bytes`, any future tear-client
    /// renderer) read this to encode Up/Down/Right/Left as
    /// `ESC O A/B/C/D` (true) or `ESC [ A/B/C/D` (false).
    #[must_use]
    pub fn cursor_keys_mode(&self) -> bool {
        self.state.cursor_keys_mode
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

/// Character-width parity with mado's parser.
///
/// These are the RED GATE for the wide-character axis. They assert what a
/// correct VT parser does with double-width glyphs, which is what mado does
/// (`unicode-width`, `Cell.width` with `0 = continuation`) and what tear did
/// NOT: `advance_cursor_after_print` was `cursor_col += 1` unconditionally.
///
/// Why this shape and not a round-trip: a `feed → to_ansi → feed` round-trip
/// through tear alone is IDENTITY even while broken, because tear was
/// internally self-consistent at 1-advance. Self-consistency is exactly what
/// makes the bug invisible from inside. So these assert against true display
/// width — the oracle — not against tear's own agreement with itself.
#[cfg(test)]
mod width_parity {
    use super::*;

    /// The founding symptom, minimal: one CJK glyph must consume TWO columns.
    /// Before the fix this reported `cursor_col == 1`.
    #[test]
    fn wide_glyph_advances_two_columns() {
        let mut g = PaneGrid::new(20, 3);
        g.feed("你".as_bytes());
        let s = g.snapshot();
        assert_eq!(s.cells[0][0].ch, '你', "lead cell holds the glyph");
        assert_eq!(s.cells[0][0].width, 2, "lead is marked double-width");
        assert_eq!(s.cells[0][1].width, 0, "col 1 is a continuation cell");
        assert_eq!(s.cursor_col, 2, "cursor advances by the glyph's WIDTH");
    }

    /// The divergence compounds: every later cell on the row is displaced.
    /// This is the mechanism behind the column-shifted decoration that
    /// survived 10+ fix attempts inside mado.
    #[test]
    fn later_cells_are_not_displaced_by_wide_glyphs() {
        let mut g = PaneGrid::new(20, 3);
        g.feed("你好X".as_bytes());
        let s = g.snapshot();
        assert_eq!(s.cells[0][0].ch, '你');
        assert_eq!(s.cells[0][2].ch, '好', "second glyph starts at col 2, not 1");
        assert_eq!(s.cells[0][4].ch, 'X', "ASCII lands at col 4, not 2");
        assert_eq!(s.cursor_col, 5);
    }

    /// A wide glyph that cannot fit before the margin wraps WHOLE — it is
    /// never split across the seam.
    #[test]
    fn wide_glyph_that_does_not_fit_wraps_whole() {
        let mut g = PaneGrid::new(20, 3);
        g.feed("A".repeat(19).as_bytes());
        g.feed("你".as_bytes());
        let s = g.snapshot();
        assert_eq!(s.cells[0][19].ch, ' ', "last col of row 0 stays blank");
        assert_eq!(s.cells[1][0].ch, '你', "glyph moved to the next row whole");
        assert_eq!(s.cells[1][1].width, 0);
    }

    /// A wide glyph landing flush against the margin parks the cursor on the
    /// LAST column, not on its own lead. Without this clamp every subsequent
    /// relative motion is off by one and CSI 6n under-reports the column.
    #[test]
    fn wide_glyph_flush_to_margin_parks_at_last_column() {
        let mut g = PaneGrid::new(20, 3);
        g.feed("A".repeat(18).as_bytes());
        g.feed("你".as_bytes());
        let s = g.snapshot();
        assert_eq!(s.cells[0][18].ch, '你');
        assert_eq!(s.cells[0][19].width, 0);
        assert_eq!(s.cursor_col, 19, "parked at the last column, not at 18");
    }

    /// Overwriting half a wide pair must not leave the other half orphaned —
    /// an orphan renders as half a glyph.
    #[test]
    fn overwriting_a_wide_pair_clears_its_orphan() {
        let mut g = PaneGrid::new(20, 3);
        g.feed("你".as_bytes());
        g.feed(b"\x1b[1;1H");
        g.feed(b"X");
        let s = g.snapshot();
        assert_eq!(s.cells[0][0].ch, 'X');
        assert_eq!(s.cells[0][0].width, 1);
        assert_eq!(
            s.cells[0][1].ch, ' ',
            "the orphaned continuation is cleared, not left as a half-glyph"
        );
        assert_eq!(s.cells[0][1].width, 1);
    }

    /// A combining mark attaches to the preceding glyph and consumes no
    /// column. Placing it in its own cell (what this parser did before)
    /// displaces every later cell on the row.
    #[test]
    fn a_combining_mark_attaches_to_the_base_cell() {
        let mut g = PaneGrid::new(20, 3);
        g.feed("e\u{301}X".as_bytes()); // e + COMBINING ACUTE + X
        let s = g.snapshot();
        assert_eq!(s.cells[0][0].ch, 'e');
        assert_eq!(
            s.cells[0][0].marks(&s.combining),
            &['\u{301}'],
            "the mark belongs to the base cell"
        );
        assert_eq!(s.cells[0][1].ch, 'X', "X is at col 1, not col 2");
        assert_eq!(s.cursor_col, 2, "a mark consumes no column");
    }

    /// Several marks stack onto one base cell.
    #[test]
    fn stacked_marks_accumulate_on_one_cell() {
        let mut g = PaneGrid::new(20, 3);
        g.feed("a\u{301}\u{308}".as_bytes());
        let s = g.snapshot();
        assert_eq!(s.cells[0][0].marks(&s.combining), &['\u{301}', '\u{308}']);
        assert_eq!(s.cursor_col, 1);
    }

    /// ★ The case mado had to fix as a REGRESSION (2026-07-30), so tear is
    /// born with it. When a wrap is pending the search starts at the last
    /// column — which for a margin-flush wide glyph is its CONTINUATION.
    /// Attaching there puts the mark on a width-0 cell where it renders
    /// nowhere; it must walk back to the lead.
    #[test]
    fn a_mark_after_a_margin_flush_wide_glyph_lands_on_the_lead() {
        let mut g = PaneGrid::new(20, 3);
        g.feed("A".repeat(18).as_bytes());
        g.feed("你\u{301}".as_bytes());
        let s = g.snapshot();
        assert_eq!(s.cells[0][18].ch, '你', "lead at col 18");
        assert_eq!(
            s.cells[0][18].marks(&s.combining),
            &['\u{301}'],
            "the mark must attach to the LEAD, not the continuation"
        );
        assert!(
            s.cells[0][19].marks(&s.combining).is_empty(),
            "the continuation owns no marks"
        );
    }

    /// A mark with no preceding glyph is dropped rather than creating a
    /// cell — matching mado.
    #[test]
    fn a_mark_at_column_zero_is_dropped() {
        let mut g = PaneGrid::new(20, 3);
        g.feed("\u{301}".as_bytes());
        let s = g.snapshot();
        assert_eq!(s.cursor_col, 0, "no column consumed");
        assert!(s.combining.is_empty(), "no table entry minted");
        assert_eq!(s.cells[0][0].ch, ' ');
    }

    /// REP (`CSI b`) repeats the BASE glyph, not the mark — which is why
    /// `combine_into_previous` must not touch `last_printed`.
    #[test]
    fn rep_after_a_mark_repeats_the_base_glyph() {
        let mut g = PaneGrid::new(20, 3);
        g.feed("e\u{301}".as_bytes());
        g.feed(b"\x1b[2b");
        let s = g.snapshot();
        assert_eq!(s.cells[0][1].ch, 'e', "REP repeats the base, not the mark");
        assert_eq!(s.cells[0][2].ch, 'e');
    }

    /// Marks must survive a replay, or a session switch silently strips
    /// every accent on screen.
    #[test]
    fn marks_survive_a_to_ansi_round_trip() {
        let mut a = PaneGrid::new(20, 3);
        a.feed("e\u{301}X".as_bytes());
        let first = a.snapshot();

        let mut b = PaneGrid::new(20, 3);
        b.feed(&first.to_ansi());
        let second = b.snapshot();

        assert_eq!(second.cells[0][0].ch, 'e');
        assert_eq!(second.cells[0][0].marks(&second.combining), &['\u{301}']);
        assert_eq!(second.cells[0][1].ch, 'X');
    }

    /// `to_ansi` must not emit continuation cells: re-feeding its output has
    /// to reproduce the same grid. Emitting the spacer would push every later
    /// glyph one column right per wide glyph on replay.
    #[test]
    fn to_ansi_round_trips_wide_glyphs_without_drift() {
        let mut a = PaneGrid::new(20, 3);
        a.feed("你好X".as_bytes());
        let first = a.snapshot();

        let mut b = PaneGrid::new(20, 3);
        b.feed(&first.to_ansi());
        let second = b.snapshot();

        for col in 0..20 {
            assert_eq!(
                first.cells[0][col].ch, second.cells[0][col].ch,
                "col {col} drifted across a to_ansi round-trip"
            );
            assert_eq!(
                first.cells[0][col].width, second.cells[0][col].width,
                "col {col} width drifted across a to_ansi round-trip"
            );
        }
    }
}

/// The Relay→Host transition (docs/SHUKEN.md; task: the DSR/DA flip blocker).
///
/// tear could not answer a VT query at all — `PaneGrid` had no response
/// state, which the espelho conformance header records as tear being a
/// RELAY whose host duty "lives one layer DOWN" in mado. After the shuken
/// flip mado has no parser, so nothing would answer and every program that
/// probes the terminal would hang.
///
/// These rows pin both halves: the machinery works as a Host, and it stays
/// completely inert as a Relay.
/// The modes a client must read from the authority.
///
/// Before this, `apply_dec_mode`'s `_ => {}` silently dropped bracketed
/// paste, sync output, focus reporting, autowrap and every mouse mode — so
/// a client had no way to learn them from tear and mado read them from its
/// own parser instead. That is correct only while mado still parses, and
/// becomes a live bug the instant this grid is authoritative.
/// Inline images reach the authority instead of vanishing.
///
/// The last flip blocker in `docs/SHUKEN.md`. `GridState` implemented no
/// DCS `hook`/`put`/`unhook`, and vte has **no APC callback at all** — it
/// enters `State::SosPmApcString` and consumes to the terminator — so every
/// sixel and every kitty image was swallowed with no error and no flag. A
/// renderer could not even learn that content had been dropped.
#[cfg(test)]
mod graphics_rows {
    use super::*;

    #[test]
    fn a_sixel_payload_reaches_the_snapshot() {
        let mut g = PaneGrid::new(80, 24);
        g.feed(b"\x1bPq#0;2;0;0;0#0~~@@vv@@~~@@~~$\x1b\\");
        let s = g.snapshot();
        assert_eq!(s.graphics.len(), 1, "the sixel must not vanish");
        assert_eq!(s.graphics[0].protocol, GraphicProtocol::Sixel);
        assert!(!s.graphics[0].data.is_empty());
        assert!(!s.graphics[0].truncated);
    }

    #[test]
    fn a_kitty_payload_reaches_the_snapshot_with_its_params_split_off() {
        let mut g = PaneGrid::new(80, 24);
        g.feed(b"\x1b_Ga=T,f=100,s=2,v=2;iVBORw0KGgo=\x1b\\");
        let s = g.snapshot();
        assert_eq!(s.graphics.len(), 1, "the kitty image must not vanish");
        let img = &s.graphics[0];
        assert_eq!(img.protocol, GraphicProtocol::Kitty);
        assert_eq!(img.params, "a=T,f=100,s=2,v=2");
        assert_eq!(img.data, b"iVBORw0KGgo=".to_vec());
    }

    /// ★ The case that breaks a naive scanner. A PTY read boundary can
    /// fall anywhere, including between `ESC` and `_`, so re-assembly must
    /// survive across `feed()` calls — the same chunk-boundary property the
    /// espelho conformance rows pin for ordinary escapes.
    #[test]
    fn an_apc_split_across_feeds_reassembles() {
        let whole = b"\x1b_Ga=T,f=100;PAYLOAD\x1b\\";
        for cut in 1..whole.len() {
            let mut g = PaneGrid::new(80, 24);
            g.feed(&whole[..cut]);
            g.feed(&whole[cut..]);
            let s = g.snapshot();
            assert_eq!(s.graphics.len(), 1, "lost the image when cut at {cut}");
            assert_eq!(s.graphics[0].data, b"PAYLOAD".to_vec(), "cut at {cut}");
            assert!(
                s.to_text_rows().iter().all(|r| r.trim().is_empty()),
                "APC bytes leaked into the grid when cut at {cut}"
            );
        }
    }

    /// A withheld `ESC` that turns out NOT to open an APC must be replayed
    /// to the parser, or the sequence it belonged to is silently lost.
    #[test]
    fn a_non_apc_escape_still_reaches_the_parser() {
        let mut g = PaneGrid::new(80, 24);
        // Split mid-escape so the ESC is withheld across the boundary.
        g.feed(b"AB\x1b");
        g.feed(b"[1;1HX");
        let s = g.snapshot();
        assert_eq!(
            s.cells[0][0].ch, 'X',
            "the CUP that followed a withheld ESC must still be honoured"
        );
    }

    #[test]
    fn an_apc_terminated_by_bel_is_accepted() {
        let mut g = PaneGrid::new(80, 24);
        g.feed(b"\x1b_Ga=T;DATA\x07");
        assert_eq!(g.snapshot().graphics.len(), 1, "BEL terminates APC too");
    }

    /// A control-only kitty command (query, delete) carries no `;` and no
    /// payload. It is real and must not be mistaken for a malformed image.
    #[test]
    fn a_kitty_control_command_without_a_payload_is_kept() {
        let mut g = PaneGrid::new(80, 24);
        g.feed(b"\x1b_Ga=d,d=A\x1b\\");
        let s = g.snapshot();
        assert_eq!(s.graphics.len(), 1);
        assert_eq!(s.graphics[0].params, "a=d,d=A");
        assert!(s.graphics[0].data.is_empty());
    }

    /// A non-kitty APC is dropped — APC is a private-use channel and an
    /// unrecognised one carries nothing we could act on.
    #[test]
    fn an_unrecognised_apc_is_dropped_without_reaching_the_grid() {
        let mut g = PaneGrid::new(80, 24);
        g.feed(b"\x1b_Zsomething-else\x1b\\after");
        let s = g.snapshot();
        assert!(s.graphics.is_empty(), "not a kitty payload");
        assert_eq!(s.cells[0][0].ch, 'a', "the text after it still lands");
    }

    /// A runaway payload is CUT and says so. Silently rendering a partial
    /// image is worse than rendering none, and an unbounded one lets a
    /// child drive the daemon out of memory.
    #[test]
    fn an_oversized_payload_is_bounded_and_flagged() {
        let mut g = PaneGrid::new(80, 24);
        g.feed(b"\x1b_Ga=T;");
        // Feed past the cap in chunks, as a real PTY would.
        let chunk = vec![b'x'; 1024 * 1024];
        for _ in 0..10 {
            g.feed(&chunk);
        }
        g.feed(b"\x1b\\");
        let s = g.snapshot();
        assert_eq!(s.graphics.len(), 1);
        assert!(s.graphics[0].truncated, "the cut must be visible");
        assert!(
            s.graphics[0].data.len() <= GRAPHIC_PAYLOAD_MAX + 1,
            "payload not bounded: {}",
            s.graphics[0].data.len()
        );
    }

    /// Graphics must not smear into the rendered text — the residue row
    /// espelho's conformance test guards for queries, applied to images.
    #[test]
    fn image_bytes_leave_no_residue_in_the_grid() {
        let mut g = PaneGrid::new(80, 24);
        g.feed(b"before|");
        g.feed(b"\x1b_Ga=T,f=100;iVBORw0KGgo=\x1b\\");
        g.feed(b"\x1bPq#0;2;0;0;0#0~~$\x1b\\");
        g.feed(b"|after");
        let row0 = g.snapshot().to_text_rows().into_iter().next().unwrap();
        assert_eq!(row0.trim_end(), "before||after");
    }
}

#[cfg(test)]
mod mode_rows {
    use super::*;

    #[test]
    fn a_fresh_pane_reports_xterm_defaults() {
        let g = PaneGrid::new(80, 24);
        let m = g.modes();
        assert!(m.autowrap.enabled(), "DECAWM is ON by default per xterm");
        assert!(m.cursor_visible.enabled());
        assert!(!m.bracketed_paste.enabled());
        assert!(!m.sync_output.enabled());
        assert!(!m.mouse.is_on());
    }

    /// The one that gates paste sanitisation.
    #[test]
    fn bracketed_paste_is_tracked() {
        let mut g = PaneGrid::new(80, 24);
        assert!(!g.modes().bracketed_paste.enabled());
        g.feed(b"\x1b[?2004h");
        assert!(g.modes().bracketed_paste.enabled(), "DEC 2004 set");
        g.feed(b"\x1b[?2004l");
        assert!(!g.modes().bracketed_paste.enabled(), "DEC 2004 reset");
    }

    #[test]
    fn the_remaining_flag_modes_are_tracked() {
        let mut g = PaneGrid::new(80, 24);
        g.feed(b"\x1b[?1004h\x1b[?2026h\x1b[?1006h\x1b[?7l\x1b[?1h\x1b[?25l");
        let m = g.modes();
        assert!(m.focus_reporting.enabled(), "DEC 1004");
        assert!(m.sync_output.enabled(), "DEC 2026");
        assert!(m.mouse_sgr.enabled(), "DEC 1006");
        assert!(!m.autowrap.enabled(), "DEC 7 reset");
        assert!(m.cursor_keys.enabled(), "DEC 1 (DECCKM)");
        assert!(!m.cursor_visible.enabled(), "DEC 25 reset");
    }

    /// Mouse levels are exclusive — the LAST one set wins. Three
    /// independent bools would let two be true at once, which no terminal
    /// can mean; the enum makes that unconstructible.
    #[test]
    fn mouse_tracking_levels_replace_rather_than_accumulate() {
        let mut g = PaneGrid::new(80, 24);
        g.feed(b"\x1b[?1000h");
        assert_eq!(g.modes().mouse, MouseTracking::Click);
        g.feed(b"\x1b[?1003h");
        assert_eq!(
            g.modes().mouse,
            MouseTracking::Motion,
            "the later level replaces the earlier one"
        );
        g.feed(b"\x1b[?1003l");
        assert_eq!(g.modes().mouse, MouseTracking::Off);
    }

    #[test]
    fn alt_screen_is_reported_as_a_mode() {
        let mut g = PaneGrid::new(80, 24);
        assert!(!g.modes().alt_screen.enabled());
        g.feed(b"\x1b[?1049h");
        assert!(g.modes().alt_screen.enabled());
        g.feed(b"\x1b[?1049l");
        assert!(!g.modes().alt_screen.enabled());
    }
}

#[cfg(test)]
mod host_role_rows {
    use super::*;

    /// ★ THE LOAD-BEARING ROW. Landing the response path must change
    /// nothing today, because mado is still parsing — if tear answered now,
    /// the child would get TWO replies and the second lands on the PTY as
    /// if the operator had typed `^[[24;80R`.
    #[test]
    fn a_relay_answers_nothing_at_all() {
        let mut g = PaneGrid::new(80, 24);
        // Every query tear knows how to answer, at once.
        g.feed(b"\x1b[6n\x1b[5n\x1b[c\x1b[>c");
        assert!(
            g.take_response().is_none(),
            "a Relay must stay byte-for-byte silent — otherwise the shipped \
             mado+tear pair produces two answers per query"
        );
    }

    #[test]
    fn a_host_answers_cursor_position_one_based() {
        let mut g = PaneGrid::new(80, 24);
        g.set_host_role(HostRole::Host);
        g.feed(b"hi\r\n");
        g.feed(b"\x1b[6n");
        let r = g.take_response().expect("host must answer CPR");
        // row 2, col 1 — 1-based, after one linefeed and a carriage return.
        assert_eq!(r, b"\x1b[2;1R".to_vec());
    }

    /// CPR reports the CLAMPED column, which is what ties this to the
    /// width work: a cursor parked on a wide glyph's lead rather than the
    /// last column would under-report here.
    #[test]
    fn a_host_reports_the_clamped_column_after_a_margin_flush_wide_glyph() {
        let mut g = PaneGrid::new(20, 3);
        g.set_host_role(HostRole::Host);
        g.feed("A".repeat(18).as_bytes());
        g.feed("你".as_bytes());
        g.feed(b"\x1b[6n");
        let r = g.take_response().expect("host must answer CPR");
        assert_eq!(r, b"\x1b[1;20R".to_vec(), "column is 1-based and clamped");
    }

    #[test]
    fn a_host_answers_device_status_and_both_device_attributes() {
        let mut g = PaneGrid::new(80, 24);
        g.set_host_role(HostRole::Host);

        g.feed(b"\x1b[5n");
        assert_eq!(g.take_response().unwrap(), TearCaps::STATUS_OK.to_vec());

        g.feed(b"\x1b[c");
        assert_eq!(g.take_response().unwrap(), TearCaps::PRIMARY_DA.to_vec());

        // `CSI > c` is a DIFFERENT query sharing a final byte with `CSI c`.
        // Dispatching on the final byte alone would answer the wrong one.
        g.feed(b"\x1b[>c");
        assert_eq!(g.take_response().unwrap(), TearCaps::SECONDARY_DA.to_vec());
    }

    /// A reply is owed to the CHILD, not painted on the screen. If a query
    /// smeared into the grid the operator would see `[24;80R` in their
    /// output — the residue row espelho's conformance test also guards.
    #[test]
    fn a_query_leaves_no_residue_in_the_rendered_grid() {
        for role in [HostRole::Relay, HostRole::Host] {
            let mut g = PaneGrid::new(80, 24);
            g.set_host_role(role);
            g.feed(b"before|");
            g.feed(b"\x1b[6n");
            g.feed(b"|after");
            let row0 = g.snapshot().to_text_rows().into_iter().next().unwrap();
            assert_eq!(
                row0.trim_end(),
                "before||after",
                "query bytes must never reach the grid ({role:?})"
            );
        }
    }

    #[test]
    fn taking_a_response_drains_it() {
        let mut g = PaneGrid::new(80, 24);
        g.set_host_role(HostRole::Host);
        g.feed(b"\x1b[5n");
        assert!(g.take_response().is_some());
        assert!(g.take_response().is_none(), "a reply is delivered once");
    }
}

/// Measurements, not assertions.
///
/// `#[ignore]` on purpose: these print timings and would be flaky as
/// gates. They exist so a perf claim about this file can be RE-MEASURED
/// rather than argued, per the fleet rule that perf decisions come from
/// profiled wall-clock and not from reading the code.
///
/// Run with:
/// ```text
/// cargo test --release -p tear-core --lib perf_measurements -- --ignored --nocapture
/// ```
///
/// ## Measured 2026-07-31 (aarch64-darwin, release)
///
/// | scrollback rows | `snapshot()` |
/// |---|---|
/// | 977 | 127 µs |
/// | 9,977 | 2.15 ms |
/// | 99,977 | **16.2 ms** |
///
/// Linear at roughly **0.16 µs/row**, which is the honest consequence of
/// `DEFAULT_SCROLLBACK_ROWS = usize::MAX`: the snapshot cost of a
/// long-lived pane is unbounded because its history is.
///
/// **Not currently a defect, and the reason matters.** `snapshot()` is not
/// on the per-frame render path — bytes reach a renderer through the
/// subscriber stream, and snapshots are taken on session switch, on
/// initial subscribe, and for MCP reads. At 100k rows a switch pays ~16 ms
/// once, which is perceptible but not a stall.
///
/// **The lever, if it ever is hot:** bound what a snapshot CARRIES rather
/// than what the grid keeps — the visible grid plus N scrollback rows —
/// or let `PaneSnapshot` borrow instead of own. Do not shrink the
/// scrollback itself; "never lose anything" is a product decision, not an
/// accident.
///
/// This measurement is also what settled `Cell`'s representation: with
/// clones this large, keeping `Cell: Copy` makes them a memcpy instead of
/// a per-cell branch. See `Cell::combining`.
#[cfg(test)]
mod perf_measurements {
    use super::*;
    use std::time::Instant;

    /// `snapshot()` clones the entire scrollback, and the default cap is
    /// `usize::MAX`. This is the cost that decided `Cell` stays `Copy`;
    /// it deserves a number rather than an intuition.
    #[test]
    #[ignore = "measurement, not an assertion"]
    fn snapshot_cost_by_scrollback_depth() {
        for rows in [1_000usize, 10_000, 100_000] {
            let mut g = PaneGrid::new(80, 24);
            for i in 0..rows {
                g.feed(format!("line {i} with some ordinary ascii payload\r\n").as_bytes());
            }
            // Warm, then measure a small batch.
            let _ = g.snapshot();
            let t = Instant::now();
            const N: u32 = 10;
            for _ in 0..N {
                let s = g.snapshot();
                std::hint::black_box(&s);
            }
            let per = t.elapsed() / N;
            let sb = g.snapshot().scrollback.len();
            println!("scrollback {sb:>7} rows -> snapshot {per:?} each");
        }
    }

    /// Does the combining table cost anything when nothing uses it? It is
    /// cloned wholesale into every snapshot.
    #[test]
    #[ignore = "measurement, not an assertion"]
    fn snapshot_cost_with_and_without_combining_marks() {
        let mut plain = PaneGrid::new(80, 24);
        let mut marked = PaneGrid::new(80, 24);
        for _ in 0..5_000 {
            plain.feed(b"plain ascii line here\r\n");
            marked.feed("ma\u{301}rked li\u{308}ne he\u{301}re\r\n".as_bytes());
        }
        for (name, g) in [("plain", &plain), ("marked", &marked)] {
            let _ = g.snapshot();
            let t = Instant::now();
            const N: u32 = 10;
            for _ in 0..N {
                std::hint::black_box(g.snapshot());
            }
            let s = g.snapshot();
            println!(
                "{name:>7}: snapshot {:?} each, combining table {} entries",
                t.elapsed() / N,
                s.combining.len()
            );
        }
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

    /// No realistic SGR form may leave UNDERLINE stuck on the pen — the
    /// "everything is underlined" regression class. Each case writes a
    /// marker `X` AFTER an underline-off (or a non-underline) sequence;
    /// the marker must not carry UNDERLINE. Covers the legacy `24`
    /// reset, `0` reset, the styled `4:N` sub-param forms, `21`, the
    /// underline-colour pair `58`/`59`, and both the semicolon and
    /// COLON extended-colour spellings (the colon form flattens with an
    /// empty colourspace slot, so a mis-consumed component can leak into
    /// the SGR walk and land on an attribute code).
    #[test]
    fn no_sgr_form_leaves_underline_stuck_on_the_pen() {
        let cases: &[(&str, &[u8])] = &[
            ("4m then 24m", b"\x1b[4mU\x1b[24mX"),
            ("4m then 0m", b"\x1b[4mU\x1b[0mX"),
            ("4:3m then 4:0m", b"\x1b[4:3mU\x1b[4:0mX"),
            ("4:3m then 24m", b"\x1b[4:3mU\x1b[24mX"),
            ("21m (double-underline)", b"\x1b[21mX"),
            ("58:2::255:0:0 then 59m", b"\x1b[58:2::255:0:0mU\x1b[59mX"),
            ("fg truecolor semicolon", b"\x1b[38;2;177;185;249mX"),
            ("fg truecolor COLON", b"\x1b[38:2::177:185:249mX"),
            ("fg 256 semicolon", b"\x1b[38;5;4mX"),
            ("fg 256 COLON", b"\x1b[38:5:4mX"),
            ("bold+italic only", b"\x1b[1;3mX"),
        ];
        let mut leaked = Vec::new();
        for (name, bytes) in cases {
            let mut g = PaneGrid::new(20, 1);
            g.feed(bytes);
            let snap = g.snapshot();
            // The marker 'X' is the LAST printed cell on row 0.
            let marker = snap.cells[0]
                .iter()
                .rev()
                .find(|c| c.ch == 'X')
                .expect("marker X present");
            if marker.attrs.contains(CellAttrs::UNDERLINE) {
                leaked.push(*name);
            }
        }
        assert!(
            leaked.is_empty(),
            "these SGR forms leave UNDERLINE stuck on the pen: {leaked:?}"
        );
    }

    /// Attrs of the last `X` printed by `seq` + `X`.
    fn marker_attrs(seq: &[u8]) -> CellAttrs {
        let mut g = PaneGrid::new(20, 1);
        let mut buf = seq.to_vec();
        buf.push(b'X');
        g.feed(&buf);
        g.snapshot().cells[0]
            .iter()
            .rev()
            .find(|c| c.ch == 'X')
            .expect("marker X present")
            .attrs
    }

    /// THE regression. `CSI > 4 ; 2 m` is xterm's XTMODKEYS
    /// (modifyOtherKeys), not SGR — Claude Code emits it at startup.
    /// Dispatching on the final byte `m` alone read it as SGR 4 + SGR 2
    /// and latched UNDERLINE + DIM onto the pen for the whole session,
    /// so every subsequent cell rendered underlined.
    #[test]
    fn xtmodkeys_is_not_sgr() {
        let attrs = marker_attrs(b"\x1b[>4;2m");
        assert!(
            !attrs.contains(CellAttrs::UNDERLINE),
            "CSI >4;2m (XTMODKEYS) must not set UNDERLINE"
        );
        assert!(
            !attrs.contains(CellAttrs::DIM),
            "CSI >4;2m (XTMODKEYS) must not set DIM"
        );
        assert_eq!(attrs, CellAttrs::NONE, "XTMODKEYS must touch no attribute");
    }

    /// The whole private-parameter namespace, not just the one sequence
    /// that bit us. A prefix in 0x3C..=0x3F shares final bytes with the
    /// standard sequences; none may execute the standard command. This
    /// guards every future `match c` arm from silently giving a private
    /// sequence a meaning.
    #[test]
    fn private_parameter_csi_never_runs_the_standard_command() {
        for seq in [
            &b"\x1b[>4;2m"[..],
            &b"\x1b[>1m"[..],
            &b"\x1b[?4m"[..],
            &b"\x1b[=4m"[..],
            &b"\x1b[<4m"[..],
        ] {
            assert_eq!(
                marker_attrs(seq),
                CellAttrs::NONE,
                "private CSI {:?} must not act as SGR",
                String::from_utf8_lossy(seq),
            );
        }

        for seq in [
            &b"\x1b[>5A"[..],
            &b"\x1b[>5C"[..],
            &b"\x1b[?5G"[..],
            &b"\x1b[>2;3H"[..],
        ] {
            let mut g = PaneGrid::new(20, 3);
            g.feed(b"\x1b[H");
            g.feed(seq);
            let snap = g.snapshot();
            assert_eq!(
                (snap.cursor_row, snap.cursor_col),
                (0, 0),
                "private CSI {:?} must not move the cursor",
                String::from_utf8_lossy(seq),
            );
        }

        let mut g = PaneGrid::new(20, 1);
        g.feed(b"keep\x1b[H\x1b[?2J\x1b[?0K");
        let row: String = g.snapshot().cells[0].iter().map(|c| c.ch).collect();
        assert!(
            row.starts_with("keep"),
            "private CSI ?J/?K must not erase; row was {row:?}"
        );
    }

    fn marker_fg(seq: &[u8]) -> Color {
        let mut g = PaneGrid::new(20, 1);
        let mut buf = seq.to_vec();
        buf.push(b'X');
        g.feed(&buf);
        g.snapshot().cells[0]
            .iter()
            .rev()
            .find(|c| c.ch == 'X')
            .expect("marker X present")
            .fg
    }

    /// The two spellings of an extended colour must agree, and neither
    /// may leak a component into the attribute walk.
    ///
    /// The colon form carries a colour-space id in slot 2 (`38:2::r:g:b`,
    /// usually empty). Flattening parameters and sub-parameters into one
    /// stream read that empty slot as RED: every channel shifted by one
    /// and the real blue fell out the end to be executed as an SGR code.
    #[test]
    fn semicolon_and_colon_extended_colour_agree() {
        let cases: &[(&[u8], &[u8], Color)] = &[
            (
                b"\x1b[38;2;248;248;242m",
                b"\x1b[38:2::248:248:242m",
                Color::new(248, 248, 242),
            ),
            (
                b"\x1b[38;2;177;185;249m",
                b"\x1b[38:2::177:185:249m",
                Color::new(177, 185, 249),
            ),
            // the channel value that used to latch UNDERLINE on
            (
                b"\x1b[38;2;4;4;4m",
                b"\x1b[38:2::4:4:4m",
                Color::new(4, 4, 4),
            ),
        ];
        for (semi, colon, want) in cases {
            assert_eq!(marker_fg(semi), *want, "semicolon form {semi:?}");
            assert_eq!(marker_fg(colon), *want, "COLON form {colon:?}");
        }
        // the 5-slot colon form (no colour-space id) is also legal
        assert_eq!(
            marker_fg(b"\x1b[38:2:10:20:30m"),
            Color::new(10, 20, 30),
            "5-slot colon truecolor"
        );
    }

    /// No extended-colour spelling may leave an attribute behind. This is
    /// the generalisation of the XTMODKEYS bug: a directive whose
    /// parameters are not fully consumed leaks them into the attribute
    /// walk, and a leaked `4` is a permanently underlined session.
    #[test]
    fn extended_colour_never_leaks_an_attribute() {
        let mut leaked = Vec::new();
        for seq in [
            &b"\x1b[38;2;4;4;4m"[..],
            &b"\x1b[38:2::4:4:4m"[..],
            &b"\x1b[48;2;4;4;4m"[..],
            &b"\x1b[48:2::4:4:4m"[..],
            &b"\x1b[38;5;4m"[..],
            &b"\x1b[38:5:4m"[..],
            &b"\x1b[48;5;4m"[..],
            // SGR 58/59 — underline COLOUR. tear has nowhere to store it,
            // but it must still be consumed or its components walk.
            &b"\x1b[58;5;4m"[..],
            &b"\x1b[58;2;4;4;4m"[..],
            &b"\x1b[58:2::255:0:0m"[..],
            &b"\x1b[59m"[..],
            // truncated / malformed forms must not hang or leak
            &b"\x1b[38m"[..],
            &b"\x1b[38;2m"[..],
            &b"\x1b[38;5m"[..],
        ] {
            if marker_attrs(seq) != CellAttrs::NONE {
                leaked.push(String::from_utf8_lossy(seq).replace('\x1b', "ESC"));
            }
        }
        assert!(
            leaked.is_empty(),
            "these forms leaked an attribute: {leaked:?}"
        );
    }

    /// `4:N` is the styled-underline sub-parameter form. tear stores a
    /// boolean, so `4:0` is off and every other style is on — but a
    /// flattened walk read `4:3` as SGR 4 THEN SGR 3, turning a curly
    /// underline into underline + italic.
    #[test]
    fn styled_underline_subparams() {
        assert!(marker_attrs(b"\x1b[4:3m").contains(CellAttrs::UNDERLINE));
        assert!(
            !marker_attrs(b"\x1b[4:3m").contains(CellAttrs::ITALIC),
            "4:3 is a curly underline, not underline + italic"
        );
        assert!(!marker_attrs(b"\x1b[4:0m").contains(CellAttrs::UNDERLINE));
        assert!(!marker_attrs(b"\x1b[4mU\x1b[4:0m").contains(CellAttrs::UNDERLINE));
    }

    /// Plain attributes and the legacy palette codes must be untouched by
    /// the parameter/sub-parameter split.
    #[test]
    fn plain_sgr_still_works() {
        assert!(marker_attrs(b"\x1b[1m").contains(CellAttrs::BOLD));
        assert!(marker_attrs(b"\x1b[3m").contains(CellAttrs::ITALIC));
        assert!(marker_attrs(b"\x1b[1;3m").contains(CellAttrs::BOLD));
        assert!(marker_attrs(b"\x1b[1;3m").contains(CellAttrs::ITALIC));
        assert_eq!(marker_attrs(b"\x1b[1;3m\x1b[0m"), CellAttrs::NONE);
        assert_eq!(
            marker_attrs(b"\x1b[1m\x1b[m"),
            CellAttrs::NONE,
            "bare ESC[m resets"
        );
        // 31 = red from the palette; 39 = default fg
        assert_eq!(marker_fg(b"\x1b[31m"), default_ansi_palette()[1]);
        assert_eq!(marker_fg(b"\x1b[31m\x1b[39m"), Color::WHITE);
    }

    /// The `?`-private modes we DO implement must keep working — the
    /// namespace split must not throw them out with the rest.
    #[test]
    fn dec_private_modes_still_dispatch() {
        let mut g = PaneGrid::new(20, 2);
        g.feed(b"\x1b[?25l");
        assert!(
            !g.snapshot().cursor_visible,
            "DECTCEM reset must hide cursor"
        );
        g.feed(b"\x1b[?25h");
        assert!(g.snapshot().cursor_visible, "DECTCEM set must show cursor");
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

    // ── DECCKM (DEC mode 1) ─────────────────────────────────────
    //
    // Pins the cursor-keys application mode tracking that mado's
    // embedded-tear input path reads to encode arrow-key bytes
    // correctly. Vim / less / htop / btop / etc. all toggle this
    // on alt-screen entry; without correct tracking the editor
    // sees the wrong cursor-key escape sequence and arrow-key
    // navigation breaks.

    #[test]
    fn cursor_keys_mode_defaults_to_false() {
        let g = PaneGrid::new(5, 1);
        assert!(!g.cursor_keys_mode());
        assert!(!g.snapshot().cursor_keys_mode);
    }

    #[test]
    fn decckm_set_via_csi_question_1_h() {
        let mut g = PaneGrid::new(5, 1);
        g.feed(b"\x1b[?1h"); // DECCKM set
        assert!(g.cursor_keys_mode());
        assert!(g.snapshot().cursor_keys_mode);
    }

    #[test]
    fn decckm_reset_via_csi_question_1_l() {
        let mut g = PaneGrid::new(5, 1);
        g.feed(b"\x1b[?1h"); // set
        g.feed(b"\x1b[?1l"); // reset
        assert!(!g.cursor_keys_mode());
        assert!(!g.snapshot().cursor_keys_mode);
    }

    #[test]
    fn decckm_survives_unrelated_modes() {
        let mut g = PaneGrid::new(5, 1);
        g.feed(b"\x1b[?1h"); // DECCKM set
        g.feed(b"\x1b[?25l"); // hide cursor (mode 25)
        g.feed(b"\x1b[?1049h"); // enter alt-screen (mode 1049)
        assert!(
            g.cursor_keys_mode(),
            "DECCKM must persist across cursor-visibility + alt-screen toggles"
        );
    }

    #[test]
    fn ris_resets_cursor_keys_mode() {
        let mut g = PaneGrid::new(5, 1);
        g.feed(b"\x1b[?1h"); // DECCKM set
        assert!(g.cursor_keys_mode());
        g.feed(b"\x1bc"); // RIS
        assert!(
            !g.cursor_keys_mode(),
            "RIS must reset DECCKM to normal mode"
        );
    }

    #[test]
    fn decckm_multi_param_csi() {
        // Some shells set multiple modes in one CSI: `CSI ? 1 ; 25 h`.
        // Both must apply.
        let mut g = PaneGrid::new(5, 1);
        g.feed(b"\x1b[?25l"); // hide first to verify mode 25 is in fact off
        g.feed(b"\x1b[?1;25h"); // set DECCKM + DECTCEM
        assert!(g.cursor_keys_mode());
        assert!(g.snapshot().cursor_visible);
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
