//! Typed pane snapshot — the wire payload that ferries a rendered
//! pane state from `tear-core` / `tear-daemon` to a consumer.
//!
//! Lives in `tear-types` (not `tear-core`) because the wire (and
//! therefore `tear-client`) needs to deserialize these without
//! pulling in the parser. The parser side (`tear_core::PaneGrid`)
//! constructs these via `PaneGrid::snapshot()`.

use serde::{Deserialize, Serialize};
use std::io::Write;

// ── Color ──────────────────────────────────────────────────────────

/// 24-bit RGB color. Default ANSI palette entries are concrete
/// values (see [`default_ansi_palette`]); SGR 38/48 5;n / 38;2;r;g;b
/// resolve to one of these via the consumer's theme. The wire only
/// ferries explicit RGB so consumers don't have to share a palette.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Color {
    pub r: u8,
    pub g: u8,
    pub b: u8,
}

impl Color {
    pub const WHITE: Self = Self { r: 255, g: 255, b: 255 };
    pub const BLACK: Self = Self { r: 0, g: 0, b: 0 };

    #[must_use]
    pub const fn new(r: u8, g: u8, b: u8) -> Self {
        Self { r, g, b }
    }
}

impl Default for Color {
    fn default() -> Self {
        Self::WHITE
    }
}

/// Standard 8-color ANSI palette (normal intensity). Ported from
/// mado's `terminal::ANSI_COLORS` — the canonical fleet palette
/// (the same one mado renders with).
pub const ANSI_COLORS: [Color; 8] = [
    Color::new(0, 0, 0),       // 0 black
    Color::new(205, 49, 49),   // 1 red
    Color::new(13, 188, 121),  // 2 green
    Color::new(229, 229, 16),  // 3 yellow
    Color::new(36, 114, 200),  // 4 blue
    Color::new(188, 63, 188),  // 5 magenta
    Color::new(17, 168, 205),  // 6 cyan
    Color::new(229, 229, 229), // 7 white
];

/// Bright ANSI palette (indices 8-15). Ported from mado.
pub const ANSI_BRIGHT_COLORS: [Color; 8] = [
    Color::new(102, 102, 102), // 8  bright black
    Color::new(241, 76, 76),   // 9  bright red
    Color::new(35, 209, 139),  // 10 bright green
    Color::new(245, 245, 67),  // 11 bright yellow
    Color::new(59, 142, 234),  // 12 bright blue
    Color::new(214, 112, 214), // 13 bright magenta
    Color::new(41, 184, 219),  // 14 bright cyan
    Color::new(255, 255, 255), // 15 bright white
];

/// Build the default 16-color ANSI palette from the const arrays.
#[must_use]
pub fn default_ansi_palette() -> [Color; 16] {
    let mut palette = [Color::BLACK; 16];
    palette[..8].copy_from_slice(&ANSI_COLORS);
    palette[8..].copy_from_slice(&ANSI_BRIGHT_COLORS);
    palette
}

/// Resolve a 256-color index (SGR 38;5;n / 48;5;n) into a concrete
/// RGB color via the given palette. Ported from mado verbatim so
/// both apps interpret 256-color indices identically.
#[must_use]
pub fn ansi_256_color(idx: u16, palette: &[Color; 16]) -> Color {
    match idx {
        0..=15 => palette[idx as usize],
        16..=231 => {
            let idx = idx - 16;
            let r_idx = idx / 36;
            let g_idx = (idx % 36) / 6;
            let b_idx = idx % 6;
            let to_byte = |i: u16| -> u8 {
                if i == 0 { 0 } else { (55 + 40 * i) as u8 }
            };
            Color::new(to_byte(r_idx), to_byte(g_idx), to_byte(b_idx))
        }
        232..=255 => {
            let v = (8 + 10 * (idx - 232)) as u8;
            Color::new(v, v, v)
        }
        _ => Color::WHITE,
    }
}

// ── CellAttrs ──────────────────────────────────────────────────────

/// Bitflags-style attribute set. Bit positions match mado's
/// `terminal::CellAttrs` so the two apps interpret SGR-derived
/// attrs identically.
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct CellAttrs(pub u8);

impl CellAttrs {
    pub const NONE: Self = Self(0);
    pub const BOLD: Self = Self(1 << 0);
    pub const ITALIC: Self = Self(1 << 1);
    pub const UNDERLINE: Self = Self(1 << 2);
    pub const BLINK: Self = Self(1 << 3);
    pub const INVERSE: Self = Self(1 << 4);
    pub const STRIKETHROUGH: Self = Self(1 << 5);
    pub const DIM: Self = Self(1 << 6);
    pub const HIDDEN: Self = Self(1 << 7);

    #[must_use]
    pub const fn contains(self, other: Self) -> bool {
        (self.0 & other.0) == other.0
    }

    pub fn insert(&mut self, other: Self) {
        self.0 |= other.0;
    }

    pub fn remove(&mut self, other: Self) {
        self.0 &= !other.0;
    }

    #[must_use]
    pub const fn is_empty(self) -> bool {
        self.0 == 0
    }

    #[must_use]
    pub const fn bits(self) -> u8 {
        self.0
    }
}

// ── Cell ───────────────────────────────────────────────────────────

/// One cell in a snapshotted pane. Carries the rendered character
/// + foreground / background colors + attrs. Width / hyperlink /
/// combining-char fields stay mado-side until Phase 2.5 ports
/// mado's full Cell wholesale.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Cell {
    pub ch: char,
    pub fg: Color,
    pub bg: Color,
    pub attrs: CellAttrs,
}

impl Cell {
    pub const BLANK: Self = Self {
        ch: ' ',
        fg: Color::WHITE,
        bg: Color::BLACK,
        attrs: CellAttrs::NONE,
    };
}

impl Default for Cell {
    fn default() -> Self {
        Self::BLANK
    }
}

// ── Snapshot ───────────────────────────────────────────────────────

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
    /// True when the alternate screen buffer is active (vim, less,
    /// htop, btop, etc. all enter this). Consumers may want to
    /// suppress scrollback rendering when alt-screen is on.
    #[serde(default)]
    pub alt_screen_active: bool,
    /// Cursor visibility (DEC mode 25). When false, renderers
    /// should not draw the cursor cell. Defaults to true (cursor
    /// shows by default per xterm semantics).
    #[serde(default = "default_true")]
    pub cursor_visible: bool,
    /// Window/tab title set via OSC 0 / OSC 2. None until first
    /// title set; clears to None on RIS.
    #[serde(default)]
    pub title: Option<String>,
    /// DECCKM (DEC mode 1) — cursor-keys application mode.
    ///
    /// When true, the running program has requested application-
    /// mode cursor keys (ESC O A/B/C/D) instead of normal-mode
    /// (ESC [ A/B/C/D). Mado's input encoder (and any other
    /// consumer that translates host keystrokes to PTY bytes)
    /// reads this to pick the right sequence so editors and
    /// pagers (vim, less, htop, …) receive the cursor keys they
    /// expect.
    ///
    /// Resets to false on RIS (ESC c), DECSTR (ESC [ ! p), and
    /// when DECCKM is explicitly reset (ESC [ ? 1 l).
    ///
    /// Serde default is `false` so wire payloads from older
    /// daemons that don't emit this field deserialize cleanly.
    #[serde(default)]
    pub cursor_keys_mode: bool,
}

fn default_true() -> bool {
    true
}

impl PaneSnapshot {
    /// Project to plain text — one String per row, blanks rendered
    /// as ASCII spaces. Drops color/attr information; useful for
    /// assertions and grep-style introspection.
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

    /// Serialize the snapshot as a stream of ANSI bytes that, when
    /// fed into a fresh VT parser, reproduces the snapshot state
    /// (cells, colors, attrs, cursor, alt-screen, cursor-visibility).
    ///
    /// The bug class this kills: a producer (tear pane) starts
    /// emitting before a consumer (mado terminal model) attaches via
    /// `subscribe_pane_bytes`. The early bytes (shell prompt, vim
    /// initial frame) reach tear's grid but never the consumer; the
    /// consumer's local model stays empty even though tear's snapshot
    /// shows the right content. Calling `to_ansi()` and feeding the
    /// result into the consumer's VT parser BEFORE the live byte
    /// stream begins guarantees the consumer's model matches the
    /// producer's grid at attach time.
    ///
    /// Long-term home: this lives in `engate` as the canonical
    /// "history replay" operation in the typed attach protocol —
    /// `EngateAttach<Synced>` is constructed by feeding `to_ansi()`
    /// bytes through the consumer's parser, then subscribing to the
    /// live stream.
    #[must_use]
    pub fn to_ansi(&self) -> Vec<u8> {
        let mut buf: Vec<u8> = Vec::with_capacity(self.rows * self.cols * 4 + 64);
        // Enter alt-screen first if the pane is in alt-screen mode
        // (vim / htop / less). Without this the cells would paint over
        // the primary screen, corrupting it when the app exits alt.
        if self.alt_screen_active {
            buf.extend_from_slice(b"\x1b[?1049h");
        }
        // Reset SGR, clear screen, home cursor.
        buf.extend_from_slice(b"\x1b[0m\x1b[2J\x1b[H");
        // Track current SGR state so we only emit deltas.
        let mut cur_fg = Color::WHITE;
        let mut cur_bg = Color::BLACK;
        let mut cur_attrs = CellAttrs::NONE;
        for (r, row) in self.cells.iter().enumerate() {
            // Move to start of this row (1-based CSI).
            let _ = write!(buf, "\x1b[{};1H", r + 1);
            for cell in row {
                if cell.attrs != cur_attrs {
                    // Attrs only get cleared by full SGR reset — emit
                    // reset + re-establish colors + new attrs.
                    buf.extend_from_slice(b"\x1b[0m");
                    cur_fg = Color::WHITE;
                    cur_bg = Color::BLACK;
                    write_sgr_attrs(&mut buf, cell.attrs);
                    cur_attrs = cell.attrs;
                }
                if cell.fg != cur_fg {
                    let _ = write!(buf, "\x1b[38;2;{};{};{}m", cell.fg.r, cell.fg.g, cell.fg.b);
                    cur_fg = cell.fg;
                }
                if cell.bg != cur_bg {
                    let _ = write!(buf, "\x1b[48;2;{};{};{}m", cell.bg.r, cell.bg.g, cell.bg.b);
                    cur_bg = cell.bg;
                }
                let mut tmp = [0u8; 4];
                buf.extend_from_slice(cell.ch.encode_utf8(&mut tmp).as_bytes());
            }
        }
        // Position cursor (CSI is 1-based).
        let _ = write!(
            buf,
            "\x1b[{};{}H",
            self.cursor_row + 1,
            self.cursor_col + 1
        );
        // Cursor visibility.
        if !self.cursor_visible {
            buf.extend_from_slice(b"\x1b[?25l");
        }
        buf
    }
}

/// Emit SGR attribute bytes for the given attr set (does NOT include
/// the leading reset — caller resets first if previous state had
/// attrs the new state doesn't). Each attr gets its own CSI sequence
/// for simplicity; size cost is negligible vs the cell payload.
#[cfg(test)]
mod to_ansi_tests {
    use super::*;

    fn snap_with(rows: usize, cols: usize, ch: char) -> PaneSnapshot {
        PaneSnapshot {
            rows,
            cols,
            cells: (0..rows)
                .map(|_| (0..cols).map(|_| Cell { ch, ..Cell::BLANK }).collect())
                .collect(),
            cursor_row: 0,
            cursor_col: 0,
            alt_screen_active: false,
            cursor_visible: true,
            title: None,
            cursor_keys_mode: false,
        }
    }

    #[test]
    fn empty_grid_emits_clear_and_home() {
        let s = snap_with(2, 3, ' ');
        let bytes = s.to_ansi();
        let text = String::from_utf8_lossy(&bytes);
        assert!(text.contains("\x1b[0m"));
        assert!(text.contains("\x1b[2J"));
        assert!(text.contains("\x1b[H"));
        assert!(text.contains("\x1b[1;1H"));
    }

    #[test]
    fn cells_appear_in_output() {
        let mut s = snap_with(1, 5, 'x');
        s.cells[0][2].ch = 'Y';
        let text = String::from_utf8_lossy(&s.to_ansi()).into_owned();
        assert!(text.contains("xxYxx"), "got: {text:?}");
    }

    #[test]
    fn cursor_position_emitted_one_based() {
        let mut s = snap_with(5, 5, ' ');
        s.cursor_row = 3;
        s.cursor_col = 2;
        let text = String::from_utf8_lossy(&s.to_ansi()).into_owned();
        assert!(text.contains("\x1b[4;3H"), "got: {text:?}");
    }

    #[test]
    fn alt_screen_active_prepends_csi_1049h() {
        let mut s = snap_with(1, 1, ' ');
        s.alt_screen_active = true;
        let bytes = s.to_ansi();
        assert!(bytes.starts_with(b"\x1b[?1049h"));
    }

    #[test]
    fn invisible_cursor_emits_csi_25l() {
        let mut s = snap_with(1, 1, ' ');
        s.cursor_visible = false;
        let text = String::from_utf8_lossy(&s.to_ansi()).into_owned();
        assert!(text.contains("\x1b[?25l"));
    }

    // ── Expanded coverage: colors ─────────────────────────────────

    #[test]
    fn fg_color_change_emits_truecolor_sgr() {
        let mut s = snap_with(1, 1, 'r');
        s.cells[0][0].fg = Color::new(255, 100, 0); // orange
        let text = String::from_utf8_lossy(&s.to_ansi()).into_owned();
        assert!(text.contains("\x1b[38;2;255;100;0m"), "got: {text:?}");
    }

    #[test]
    fn bg_color_change_emits_truecolor_sgr() {
        let mut s = snap_with(1, 1, ' ');
        s.cells[0][0].bg = Color::new(0, 50, 100);
        let text = String::from_utf8_lossy(&s.to_ansi()).into_owned();
        assert!(text.contains("\x1b[48;2;0;50;100m"), "got: {text:?}");
    }

    #[test]
    fn both_fg_and_bg_change_in_one_cell() {
        let mut s = snap_with(1, 1, '!');
        s.cells[0][0].fg = Color::new(10, 20, 30);
        s.cells[0][0].bg = Color::new(200, 150, 100);
        let text = String::from_utf8_lossy(&s.to_ansi()).into_owned();
        assert!(text.contains("\x1b[38;2;10;20;30m"));
        assert!(text.contains("\x1b[48;2;200;150;100m"));
        assert!(text.contains('!'));
    }

    // ── Expanded coverage: each CellAttrs flag ────────────────────

    #[test]
    fn each_cellattr_flag_emits_matching_sgr() {
        let cases: &[(CellAttrs, &str)] = &[
            (CellAttrs::BOLD,          "\x1b[1m"),
            (CellAttrs::DIM,           "\x1b[2m"),
            (CellAttrs::ITALIC,        "\x1b[3m"),
            (CellAttrs::UNDERLINE,     "\x1b[4m"),
            (CellAttrs::BLINK,         "\x1b[5m"),
            (CellAttrs::INVERSE,       "\x1b[7m"),
            (CellAttrs::HIDDEN,        "\x1b[8m"),
            (CellAttrs::STRIKETHROUGH, "\x1b[9m"),
        ];
        for (attr, expected_sgr) in cases {
            let mut s = snap_with(1, 1, 'a');
            s.cells[0][0].attrs = *attr;
            let text = String::from_utf8_lossy(&s.to_ansi()).into_owned();
            assert!(
                text.contains(expected_sgr),
                "attr {:?} should emit {:?}, got {text:?}",
                attr,
                expected_sgr
            );
        }
    }

    #[test]
    fn combined_attrs_emit_all_sgr_codes() {
        let mut s = snap_with(1, 1, 'a');
        let mut combined = CellAttrs::NONE;
        combined.insert(CellAttrs::BOLD);
        combined.insert(CellAttrs::UNDERLINE);
        combined.insert(CellAttrs::ITALIC);
        s.cells[0][0].attrs = combined;
        let text = String::from_utf8_lossy(&s.to_ansi()).into_owned();
        assert!(text.contains("\x1b[1m"));
        assert!(text.contains("\x1b[3m"));
        assert!(text.contains("\x1b[4m"));
    }

    // ── Expanded coverage: SGR delta-encoding ─────────────────────

    #[test]
    fn identical_runs_do_not_re_emit_sgr() {
        // Three cells with the same SGR state should emit the SGR
        // sequence at most once for the row, not three times.
        let mut s = snap_with(1, 3, 'x');
        for c in &mut s.cells[0] {
            c.fg = Color::new(50, 100, 150);
        }
        let text = String::from_utf8_lossy(&s.to_ansi()).into_owned();
        let count = text.matches("\x1b[38;2;50;100;150m").count();
        assert_eq!(count, 1, "expected 1 fg SGR for identical run, got {count}");
    }

    #[test]
    fn fg_change_mid_row_re_emits_sgr_once() {
        let mut s = snap_with(1, 4, 'a');
        s.cells[0][0].fg = Color::new(255, 0, 0);
        s.cells[0][1].fg = Color::new(255, 0, 0);
        s.cells[0][2].fg = Color::new(0, 255, 0);
        s.cells[0][3].fg = Color::new(0, 255, 0);
        let text = String::from_utf8_lossy(&s.to_ansi()).into_owned();
        assert_eq!(text.matches("\x1b[38;2;255;0;0m").count(), 1);
        assert_eq!(text.matches("\x1b[38;2;0;255;0m").count(), 1);
    }

    // ── Expanded coverage: layout edge cases ──────────────────────

    #[test]
    fn each_row_gets_explicit_cursor_position() {
        let s = snap_with(3, 2, '.');
        let text = String::from_utf8_lossy(&s.to_ansi()).into_owned();
        // Row 1, row 2, row 3 each emitted with CSI <r>;1H.
        assert!(text.contains("\x1b[1;1H"));
        assert!(text.contains("\x1b[2;1H"));
        assert!(text.contains("\x1b[3;1H"));
    }

    #[test]
    fn utf8_multibyte_chars_round_trip() {
        let mut s = snap_with(1, 5, '·');
        s.cells[0][0].ch = '日';
        s.cells[0][1].ch = '本';
        s.cells[0][2].ch = '語';
        let text = String::from_utf8_lossy(&s.to_ansi()).into_owned();
        assert!(text.contains("日本語"), "got: {text:?}");
    }

    #[test]
    fn alt_screen_plus_cursor_hidden_combine() {
        let mut s = snap_with(1, 1, ' ');
        s.alt_screen_active = true;
        s.cursor_visible = false;
        let bytes = s.to_ansi();
        let text = String::from_utf8_lossy(&bytes);
        // alt-screen prelude first.
        assert!(bytes.starts_with(b"\x1b[?1049h"));
        // cursor-hide somewhere after.
        assert!(text.contains("\x1b[?25l"));
        // Ordering invariant: cursor-hide comes after final cursor
        // position so the hidden cursor sits at the recorded coords
        // (matters when the consumer later toggles visible again).
        let pos_idx = text.find("\x1b[1;1H").unwrap();
        let hide_idx = text.find("\x1b[?25l").unwrap();
        assert!(hide_idx > pos_idx, "cursor hide must follow position");
    }

    #[test]
    fn wide_grid_emits_proportional_bytes() {
        // 80x24 = 1920 cells. Output should be at least that many
        // chars (one byte per cell minimum). Sanity check that we
        // don't accidentally truncate or omit rows.
        let s = snap_with(24, 80, '*');
        let bytes = s.to_ansi();
        assert!(bytes.len() >= 1920, "expected >=1920 bytes, got {}", bytes.len());
        // All 24 row-position CSI sequences present.
        let text = String::from_utf8_lossy(&bytes);
        for row in 1..=24 {
            let csi = format!("\x1b[{row};1H");
            assert!(text.contains(&csi), "row {row} CSI missing");
        }
    }
}

fn write_sgr_attrs(buf: &mut Vec<u8>, attrs: CellAttrs) {
    if attrs.contains(CellAttrs::BOLD)          { buf.extend_from_slice(b"\x1b[1m"); }
    if attrs.contains(CellAttrs::DIM)           { buf.extend_from_slice(b"\x1b[2m"); }
    if attrs.contains(CellAttrs::ITALIC)        { buf.extend_from_slice(b"\x1b[3m"); }
    if attrs.contains(CellAttrs::UNDERLINE)     { buf.extend_from_slice(b"\x1b[4m"); }
    if attrs.contains(CellAttrs::BLINK)         { buf.extend_from_slice(b"\x1b[5m"); }
    if attrs.contains(CellAttrs::INVERSE)       { buf.extend_from_slice(b"\x1b[7m"); }
    if attrs.contains(CellAttrs::HIDDEN)        { buf.extend_from_slice(b"\x1b[8m"); }
    if attrs.contains(CellAttrs::STRIKETHROUGH) { buf.extend_from_slice(b"\x1b[9m"); }
}
