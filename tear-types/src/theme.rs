//! Theme — colors + typography for tear's status bar / pane borders /
//! message areas. Operators pick from a named theme or roll their own;
//! the canonical fleet theme is DERIVED from `ishou_tokens::FleetDefaults`
//! — the SAME source mado reads — so a palette change in ishou propagates
//! to tear and mado together by construction (they can never drift).

use serde::{Deserialize, Serialize};

/// Per-tear theme. Stores hex strings rather than typed Color values
/// so the serde wire format stays stable across renderer changes.
/// At runtime the in-process backend resolves these via
/// `ishou-tokens` semantics; the tmux backend writes them into the
/// rendered tmux.conf as `colour#XXXXXX`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct TearTheme {
    /// Theme name — typically one of `"vellum"` (the fleet prescribed
    /// default), `"nord"`, `"solarized-dark"`, `"gruvbox-dark"`, or
    /// `"custom"` for inline-override themes.
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
    /// The fleet prescribed theme — DERIVED from
    /// `ishou_tokens::FleetDefaults::prescribed()` (today: Vellum, the
    /// warm aged-paper Nord-matte). This is the SAME source mado's
    /// `FleetThemedConfig::from_fleet` reads, so tear and mado converge
    /// on identical colors by construction. A fleet rebrand touches
    /// `FleetDefaults::prescribed()` / `FleetTheme::prescribed_default()`
    /// and moves both at the next compile.
    fn default() -> Self {
        Self::from_fleet(&ishou_tokens::FleetDefaults::prescribed())
    }
}

impl TearTheme {
    /// Derive a `TearTheme` from `ishou_tokens::FleetDefaults` — the
    /// canonical fleet-themed constructor. Mirrors the pattern mado uses
    /// in `impl FleetThemedConfig for MadoConfig` (mado/src/config.rs):
    /// resolve the fleet theme to its BORN ishou tokens, then map each
    /// surface onto tear's status-bar / pane-border / message fields.
    ///
    /// What derives from where (all from `fd.theme.resolve()`):
    ///
    /// | tear field                  | ishou `ResolvedTheme` source     |
    /// |-----------------------------|----------------------------------|
    /// | `name`                      | `resolved.name`                  |
    /// | `fg` / `inactive_fg`        | `resolved.foreground`            |
    /// | `bg`                        | `resolved.background`            |
    /// | `active_bg` / `border_active` | `ansi_16[14]` (bright cyan / frost) |
    /// | `active_fg` / `message_fg`  | `resolved.background` (inverse)  |
    /// | `inactive_bg`               | `ansi_16[0]` (surface / night)   |
    /// | `border_inactive`           | `ansi_16[8]` (bright black / divider) |
    /// | `message_bg`                | `ansi_16[3]` (yellow / aurora)   |
    ///
    /// The active/message accents pick the SAME semantic slots the old
    /// hard-coded `nord()` used (frost-2 → ANSI bright-cyan, aurora-yellow
    /// → ANSI yellow), so the *roles* are preserved while the actual hex
    /// now flows from the fleet truth.
    #[must_use]
    pub fn from_fleet(fd: &ishou_tokens::FleetDefaults) -> Self {
        let resolved = fd.theme.resolve();
        let ansi = &resolved.ansi_16;
        Self {
            name: resolved.name.clone(),
            fg: HexColor(resolved.foreground.clone()),
            bg: HexColor(resolved.background.clone()),
            // Active window: inverse text (bg) on the frost/cyan accent.
            active_fg: HexColor(resolved.background.clone()),
            active_bg: HexColor(ansi[14].clone()), // bright cyan (frost)
            // Inactive window: foreground text on the night surface.
            inactive_fg: HexColor(resolved.foreground.clone()),
            inactive_bg: HexColor(ansi[0].clone()), // ANSI-0 surface (night)
            border_active: HexColor(ansi[14].clone()), // bright cyan (frost)
            border_inactive: HexColor(ansi[8].clone()), // bright black (divider)
            // Message line: inverse text (bg) on the aurora-yellow accent.
            message_fg: HexColor(resolved.background.clone()),
            message_bg: HexColor(ansi[3].clone()), // yellow (aurora)
        }
    }

    /// Classic-Nord named theme (Polar Night / Snow Storm / Frost). This
    /// is the EXPLICIT `"nord"` palette an operator selects by name — NOT
    /// the fleet default (which is `from_fleet`, today Vellum). Kept as a
    /// first-class option so `name = "nord"` resolves to true classic Nord
    /// rather than the warm Vellum matte. Values mirror
    /// `ishou_tokens` Nord (`PlemeDark`/`ResolvedTheme::pleme_dark`) intent.
    #[must_use]
    pub fn nord() -> Self {
        let resolved = ishou_tokens::FleetTheme::PlemeDark.resolve();
        let ansi = &resolved.ansi_16;
        Self {
            name: "nord".into(),
            fg: HexColor(resolved.foreground.clone()),
            bg: HexColor(resolved.background.clone()),
            active_fg: HexColor(resolved.background.clone()),
            active_bg: HexColor(ansi[14].clone()), // bright cyan (frost-0)
            inactive_fg: HexColor(resolved.foreground.clone()),
            inactive_bg: HexColor(ansi[0].clone()),
            border_active: HexColor(ansi[14].clone()),
            border_inactive: HexColor(ansi[8].clone()), // polar-night-3
            message_fg: HexColor(resolved.background.clone()),
            message_bg: HexColor(ansi[3].clone()), // aurora-yellow
        }
    }
}

/// Hex color — `"#rgb"` or `"#rrggbb"`. The transparent newtype keeps
/// serde output as plain strings.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct HexColor(pub String);

#[cfg(test)]
mod tests {
    use super::*;

    /// The default theme is DERIVED from the fleet prescribed palette —
    /// not a hand-pinned constant. This is the convergence guarantee:
    /// touching `FleetDefaults::prescribed()` moves tear and mado together.
    #[test]
    fn default_equals_from_fleet_prescribed() {
        let prescribed =
            TearTheme::from_fleet(&ishou_tokens::FleetDefaults::prescribed());
        assert_eq!(TearTheme::default(), prescribed);
    }

    /// Tear's default colors come from the SAME ishou `ResolvedTheme`
    /// surfaces mado reads — proving the two converge by construction.
    #[test]
    fn default_colors_match_ishou_resolved_theme() {
        let fd = ishou_tokens::FleetDefaults::prescribed();
        let resolved = fd.theme.resolve();
        let theme = TearTheme::default();

        // Name + the load-bearing surfaces flow straight from ishou.
        assert_eq!(theme.name, resolved.name);
        assert_eq!(theme.fg.0, resolved.foreground);
        assert_eq!(theme.bg.0, resolved.background);
        // Accents pull the documented ANSI slots (frost/cyan, aurora-yellow).
        assert_eq!(theme.active_bg.0, resolved.ansi_16[14]);
        assert_eq!(theme.border_active.0, resolved.ansi_16[14]);
        assert_eq!(theme.message_bg.0, resolved.ansi_16[3]);
        // Today the fleet prescribed theme is Vellum (warm Nord-matte) —
        // NOT classic Nord. Tear converges onto the fleet truth.
        assert_eq!(resolved.name, "vellum");
    }

    /// The explicit `"nord"` named theme stays classic Nord (distinct
    /// from the Vellum default) and still sources its hex from ishou.
    #[test]
    fn nord_is_classic_nord_distinct_from_default() {
        let nord = TearTheme::nord();
        assert_eq!(nord.name, "nord");
        // Classic Nord background differs from the Vellum default.
        assert_ne!(nord.bg, TearTheme::default().bg);
    }
}
