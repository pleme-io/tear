//! Typed pane snapshot — the wire payload that ferries a rendered
//! pane state from `tear-core` / `tear-daemon` to a consumer.
//!
//! Lives in `tear-types` (not `tear-core`) because the wire (and
//! therefore `tear-client`) needs to deserialize these without
//! pulling in the parser. The parser side (`tear_core::PaneGrid`)
//! constructs these via `PaneGrid::snapshot()`.

use serde::{Deserialize, Serialize};

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
}
