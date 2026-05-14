//! Theme — colors + typography for tear's status bar / pane borders /
//! message areas. Operators pick from a named theme or roll their own;
//! the canonical fleet theme references `ishou-tokens` directly so a
//! palette change in ishou propagates fleet-wide.

use serde::{Deserialize, Serialize};

/// Per-tear theme. Stores hex strings rather than typed Color values
/// so the serde wire format stays stable across renderer changes.
/// At runtime the in-process backend resolves these via
/// `ishou-tokens` semantics; the tmux backend writes them into the
/// rendered tmux.conf as `colour#XXXXXX`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct TearTheme {
    /// Theme name — typically one of `"nord"`, `"solarized-dark"`,
    /// `"gruvbox-dark"`, or `"custom"` for inline-override themes.
    pub name: String,
    /// Foreground color (status bar text default).
    pub fg: HexColor,
    /// Background color (status bar background default).
    pub bg: HexColor,
    /// Accent for the active window's segment.
    pub active_fg: HexColor,
    pub active_bg: HexColor,
    /// Accent for inactive windows.
    pub inactive_fg: HexColor,
    pub inactive_bg: HexColor,
    /// Pane border color when the pane is focused.
    pub border_active: HexColor,
    /// Pane border color when the pane is not focused.
    pub border_inactive: HexColor,
    /// Message area (e.g. tmux `command-prompt` line) colors.
    pub message_fg: HexColor,
    pub message_bg: HexColor,
}

impl Default for TearTheme {
    fn default() -> Self {
        Self::nord()
    }
}

impl TearTheme {
    /// The canonical pleme-io Nord theme. Mirrors
    /// `ishou-tokens::PaletteV2::nord_*` so a fleet-wide palette
    /// bump propagates to tear's status bar at the next config reload.
    #[must_use]
    pub fn nord() -> Self {
        Self {
            name: "nord".into(),
            fg: HexColor("#eceff4".into()),    // snow-storm-3
            bg: HexColor("#2e3440".into()),    // polar-night-0
            active_fg: HexColor("#2e3440".into()),
            active_bg: HexColor("#88c0d0".into()), // frost-2
            inactive_fg: HexColor("#d8dee9".into()),
            inactive_bg: HexColor("#3b4252".into()),
            border_active: HexColor("#88c0d0".into()),
            border_inactive: HexColor("#4c566a".into()),
            message_fg: HexColor("#2e3440".into()),
            message_bg: HexColor("#ebcb8b".into()), // aurora-yellow
        }
    }
}

/// Hex color — `"#rgb"` or `"#rrggbb"`. The transparent newtype keeps
/// serde output as plain strings.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct HexColor(pub String);
