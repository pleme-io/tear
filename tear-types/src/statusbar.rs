//! Typed status-bar model.
//!
//! tmux's status bar is configured via format strings full of
//! `#{...}` expressions. Tear models it as a typed list of
//! [`Segment`]s — each segment is one composable widget. The
//! tear-tmux-backend reduces the typed list to a tmux format string;
//! the in-process backend (which mado embeds) renders the segments
//! directly. Operators get autocomplete + type-checked status bars.

use serde::{Deserialize, Serialize};

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
    Shell {
        cmd: String,
        interval_seconds: u32,
    },
    /// Conditional: render `then` if `cond` evaluates non-empty,
    /// otherwise `else`. `cond` is a `#{...}`-style condition for
    /// tmux compatibility.
    If {
        cond: String,
        then: Box<Segment>,
        otherwise: Box<Segment>,
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
