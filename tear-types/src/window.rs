//! Window — a named tab inside a session, holding a layout tree.

use serde::{Deserialize, Serialize};

use crate::{id::WindowId, layout::LayoutNode};

/// One window: a named tab that holds a binary-tree layout of panes.
/// Windows are created/destroyed independently of their parent
/// session and have their own active-pane focus.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct TearWindow {
    pub id: WindowId,
    /// Display name shown in the status bar's window list.
    pub name: String,
    /// Pane layout. Pane ids referenced here must exist in the
    /// parent session's pane map.
    pub layout: LayoutNode,
    /// Which pane gets keystrokes. Must be a leaf in `layout`.
    pub active_pane: crate::id::PaneId,
    /// Window size in terminal cells (cols, rows).
    pub size_cells: (u16, u16),
    /// Lifecycle state.
    pub state: WindowState,
}

/// Window lifecycle states.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum WindowState {
    /// At least one pane is running.
    Active,
    /// All panes have exited. Window may be kept open per
    /// `remain-on-exit` semantics.
    Closed,
}
