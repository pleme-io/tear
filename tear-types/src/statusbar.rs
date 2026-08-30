//! Typed status-bar model.
//!
//! tmux's status bar is configured via format strings full of
//! `#{...}` expressions. Tear models it as a typed list of
//! [`Segment`]s — each segment is one composable widget. The
//! tear-tmux-backend reduces the typed list to a tmux format string;
//! the in-process backend (which mado embeds) renders the segments
//! directly. Operators get autocomplete + type-checked status bars.

use ishou_tokens::{Signal, SignalMode, TearSignals};
use serde::{Deserialize, Serialize};

/// How a signal segment renders into glyphs — the typed mirror of
/// [`ishou_tokens::SignalMode`]. A roomy status bar picks `Emoji`; a
/// tight, fixed-width bar picks `Glyph` (single-cell, never misaligns);
/// a no-emoji terminal / log picks `Label`. The *vocabulary* (which mark
/// means "zoomed") is fleet-uniform regardless of mode — it comes from
/// [`TearSignals`].
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SignalRenderMode {
    /// Emoji-first (may be two cells wide). Roomy bars, notifications.
    Emoji,
    /// Single-width geometric glyph. Tight / dense status columns.
    Glyph,
    /// Plain-text label. No-emoji terminals, logs, a11y.
    Label,
}

impl SignalRenderMode {
    /// Map to the underlying [`ishou_tokens::SignalMode`].
    #[must_use]
    pub fn to_ishou(self) -> SignalMode {
        match self {
            Self::Emoji => SignalMode::Emoji,
            Self::Glyph => SignalMode::Glyph,
            Self::Label => SignalMode::Label,
        }
    }
}

impl Default for SignalRenderMode {
    /// Emoji is the fleet default — the directive asks for emoji-based
    /// communication; tight bars opt down to `Glyph` explicitly.
    fn default() -> Self {
        Self::Emoji
    }
}

/// A semantic name for one fleet/tear status signal. Each variant maps
/// to exactly one field of [`TearSignals`] (which composes the shared
/// [`ishou_tokens::FleetSignals`] vocabulary) — so an operator writes
/// `Segment::Signal { kind: TearSignalKind::SessionActive, .. }` and gets
/// the fleet-uniform `🌊` / `≈` / `active` triple instead of hand-typing
/// an emoji into a [`Segment::Text`]. Touching the ishou atlas moves the
/// glyph fleet-wide on the next compile; tear never hardcodes the mark.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum TearSignalKind {
    // ── shared fleet session/multiplexer vocabulary ──────────────
    /// An attached, live session — the fleet `🌊` tide mark.
    SessionActive,
    /// A detached / sleeping session.
    SessionDetached,
    /// A zoomed / maximized pane.
    PaneZoomed,
    /// The multiplexer prefix key is armed (awaiting a command key).
    PrefixArmed,
    // ── shared connectivity ──────────────────────────────────────
    /// Connected / healthy.
    Online,
    /// Disconnected / down.
    Offline,
    /// Degraded / partial.
    Degraded,
    /// Terminal bell / attention.
    Bell,
    // ── shared identity ──────────────────────────────────────────
    /// The pleme-io fleet mark — Nord frost `❄`.
    FleetMark,
    // ── tear-specific vocabulary ─────────────────────────────────
    /// A window (a tab / group of panes).
    Window,
    /// A horizontal split.
    SplitHorizontal,
    /// A vertical split.
    SplitVertical,
    /// Input synchronized across panes (send-to-all).
    SyncPanes,
    /// Copy-mode / scrollback navigation active.
    CopyMode,
}

impl TearSignalKind {
    /// The [`Signal`] triple this kind names, drawn from the prescribed
    /// [`TearSignals`] atlas. The single source of truth for tear's
    /// status glyphs — no literal emoji lives in tear's source.
    #[must_use]
    pub fn signal(self) -> Signal {
        let s = TearSignals::prescribed();
        match self {
            Self::SessionActive => s.fleet.session_active,
            Self::SessionDetached => s.fleet.session_detached,
            Self::PaneZoomed => s.fleet.pane_zoomed,
            Self::PrefixArmed => s.fleet.prefix_armed,
            Self::Online => s.fleet.online,
            Self::Offline => s.fleet.offline,
            Self::Degraded => s.fleet.degraded,
            Self::Bell => s.fleet.bell,
            Self::FleetMark => s.fleet.fleet_mark,
            Self::Window => s.window,
            Self::SplitHorizontal => s.split_horizontal,
            Self::SplitVertical => s.split_vertical,
            Self::SyncPanes => s.sync_panes,
            Self::CopyMode => s.copy_mode,
        }
    }

    /// Render this kind's glyph at `mode`, falling back so a chosen field
    /// that is empty (e.g. a bare-tier emoji) still yields a non-empty
    /// mark rather than vanishing from the bar.
    #[must_use]
    pub fn render(self, mode: SignalRenderMode) -> &'static str {
        self.signal().render_or_fallback(mode.to_ishou())
    }
}

/// Where a segment sits relative to its bar.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SegmentAlignment {
    Left,
    Center,
    Right,
}

/// One status-bar widget. Each variant evaluates to a string at
/// render time; the bar concatenates segments aligned per
/// `alignment`. Colors and attrs come from the active
/// [`crate::TearTheme`].
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum Segment {
    /// Literal text — common for separators / icons.
    Text { value: String },
    /// The current session's name.
    SessionName,
    /// The current window's name.
    WindowName,
    /// The active pane's command (e.g. `"zsh"`, `"nvim"`).
    PaneCommand,
    /// Current working directory (basename).
    PaneCwdBasename,
    /// Current time, formatted via strftime.
    Time { format: String },
    /// Hostname (long or short).
    Hostname { short: bool },
    /// User-defined shell-command output, refreshed every `interval`
    /// seconds. The backend caches the most recent value.
    Shell { cmd: String, interval_seconds: u32 },
    /// Conditional: render `then` if `cond` evaluates non-empty,
    /// otherwise `else`. `cond` is a `#{...}`-style condition for
    /// tmux compatibility.
    If {
        cond: String,
        then: Box<Segment>,
        otherwise: Box<Segment>,
    },
    /// A typed fleet status signal — a semantic glyph from the
    /// [`TearSignals`] / [`ishou_tokens::FleetSignals`] atlas (session
    /// active `🌊`, pane zoomed `🔍`, prefix armed `⌨️`, sync on `🔗`,
    /// copy-mode `📜`, …). Replaces hand-typed emoji in [`Segment::Text`]
    /// with the fleet-uniform vocabulary; the glyph moves fleet-wide when
    /// the ishou atlas changes. `mode` selects emoji / single-width glyph
    /// / plain label for the bar's width budget. Pair inside a
    /// [`Segment::If`] (tmux conditional) to show the signal only when the
    /// state is active.
    ///
    /// Field is named `signal` (not `kind`) to avoid colliding with the
    /// enum's `#[serde(tag = "kind")]` discriminant key.
    Signal {
        signal: TearSignalKind,
        #[serde(default)]
        mode: SignalRenderMode,
    },
}

/// One side (left | center | right) of a status bar. Held as an
/// ordered Vec — segments render in order with theme separators in
/// between.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct StatusBar {
    /// Segments rendered on the left.
    #[serde(default)]
    pub left: Vec<Segment>,
    /// Segments rendered in the centre.
    #[serde(default)]
    pub center: Vec<Segment>,
    /// Segments rendered on the right.
    #[serde(default)]
    pub right: Vec<Segment>,
    /// Refresh interval for any time-varying segments (clock, shell
    /// segments). Seconds. tmux's `status-interval`.
    #[serde(default = "default_interval")]
    pub refresh_interval_seconds: u32,
    /// Whether the bar is rendered at all. tmux's `status off`.
    #[serde(default = "default_visible")]
    pub visible: bool,
}

fn default_interval() -> u32 {
    5
}
fn default_visible() -> bool {
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Tear's status glyphs are PINNED to the ishou atlas — no literal
    /// emoji lives in tear's source. If the atlas changes a mark, this
    /// test fails BEFORE the operator-visible bar silently drifts, and
    /// the fix is to update the atlas (the canonical vocabulary), not
    /// tear. The 🌊 tide / 🔍 zoom / ⌨️ prefix / 🔗 sync / 📜 copy-mode
    /// marks are exactly the fleet convention.
    #[test]
    fn signal_kinds_resolve_from_the_fleet_atlas() {
        let atlas = TearSignals::prescribed();
        // shared fleet session vocabulary
        assert_eq!(
            TearSignalKind::SessionActive.signal(),
            atlas.fleet.session_active
        );
        assert_eq!(TearSignalKind::PaneZoomed.signal(), atlas.fleet.pane_zoomed);
        assert_eq!(
            TearSignalKind::PrefixArmed.signal(),
            atlas.fleet.prefix_armed
        );
        // tear-specific vocabulary
        assert_eq!(TearSignalKind::Window.signal(), atlas.window);
        assert_eq!(TearSignalKind::SyncPanes.signal(), atlas.sync_panes);
        assert_eq!(TearSignalKind::CopyMode.signal(), atlas.copy_mode);
    }

    /// The active-session mark is the established fleet 🌊 tide (emoji)
    /// and `≈` (single-width glyph) — adoption is drift-free vs
    /// mado/tear/praça.
    #[test]
    fn active_session_is_the_fleet_tide_mark() {
        assert_eq!(
            TearSignalKind::SessionActive.render(SignalRenderMode::Emoji),
            "🌊"
        );
        assert_eq!(
            TearSignalKind::SessionActive.render(SignalRenderMode::Glyph),
            "≈"
        );
    }

    /// Mode selection picks the right column of the triple, and a tight
    /// bar gets a single-cell glyph for every signal.
    #[test]
    fn glyph_mode_is_single_width_for_every_kind() {
        for kind in [
            TearSignalKind::SessionActive,
            TearSignalKind::SessionDetached,
            TearSignalKind::PaneZoomed,
            TearSignalKind::PrefixArmed,
            TearSignalKind::Online,
            TearSignalKind::Offline,
            TearSignalKind::Degraded,
            TearSignalKind::Bell,
            TearSignalKind::FleetMark,
            TearSignalKind::Window,
            TearSignalKind::SplitHorizontal,
            TearSignalKind::SplitVertical,
            TearSignalKind::SyncPanes,
            TearSignalKind::CopyMode,
        ] {
            let glyph = kind.render(SignalRenderMode::Glyph);
            assert_eq!(
                glyph.chars().count(),
                1,
                "{kind:?} glyph must be single-width, got {glyph:?}"
            );
        }
    }

    /// The signal segment round-trips through serde: the enum tag is the
    /// `kind` key (`"signal"`), the signal name is the `signal` key
    /// (kebab-case), and `mode` defaults to emoji when omitted.
    #[test]
    fn signal_segment_serde_round_trips_with_default_mode() {
        let seg = Segment::Signal {
            signal: TearSignalKind::PaneZoomed,
            mode: SignalRenderMode::default(),
        };
        let json = serde_json::to_string(&seg).unwrap();
        assert!(json.contains("\"kind\":\"signal\""), "tag: {json}");
        assert!(
            json.contains("\"signal\":\"pane-zoomed\""),
            "signal: {json}"
        );
        let back: Segment = serde_json::from_str(&json).unwrap();
        assert_eq!(back, seg);

        // `mode` omitted → defaults to emoji (the fleet default).
        let no_mode: Segment =
            serde_json::from_str(r#"{"kind":"signal","signal":"sync-panes"}"#).unwrap();
        assert_eq!(
            no_mode,
            Segment::Signal {
                signal: TearSignalKind::SyncPanes,
                mode: SignalRenderMode::Emoji,
            }
        );
    }
}
