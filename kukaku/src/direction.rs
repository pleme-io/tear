//! Pane split direction + orientation.

use serde::{Deserialize, Serialize};

/// Direction a new pane appears relative to its split origin.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Direction {
    /// Above the originating pane (creates a Horizontal split — the
    /// terminology mirrors tmux's `split-window -b -v`).
    Above,
    /// Below the originating pane (default for tmux's `C-b "`).
    Below,
    /// Left of the originating pane.
    Left,
    /// Right of the originating pane (default for tmux's `C-b %`).
    Right,
}

impl Direction {
    /// The split orientation this direction creates. A new pane to
    /// the right introduces a vertical split (the cells of cells are
    /// arranged side by side); above/below introduces a horizontal
    /// split (stacked rows).
    #[must_use]
    pub const fn orientation(self) -> SplitOrientation {
        match self {
            Self::Above | Self::Below => SplitOrientation::Horizontal,
            Self::Left | Self::Right => SplitOrientation::Vertical,
        }
    }
}

/// How a [`crate::LayoutNode::Split`] divides its area.
///
/// "Horizontal" splits stack panes vertically (one above the other);
/// "Vertical" splits sit side by side. This matches the tmux
/// terminology where `split-window -h` produces side-by-side and
/// `split-window -v` produces top-and-bottom. mado uses the same
/// definitions internally; the shared types keep both apps consistent.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SplitOrientation {
    Horizontal,
    Vertical,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn direction_orientation_matches_tmux_semantics() {
        assert_eq!(Direction::Above.orientation(), SplitOrientation::Horizontal);
        assert_eq!(Direction::Below.orientation(), SplitOrientation::Horizontal);
        assert_eq!(Direction::Left.orientation(), SplitOrientation::Vertical);
        assert_eq!(Direction::Right.orientation(), SplitOrientation::Vertical);
    }
}
