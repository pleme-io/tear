//! `tear-types` — pure typed domain for the tear multiplexer.
//!
//! Owns the typed primitives ([`TearSession`], [`TearWindow`],
//! [`TearPane`], [`LayoutNode`], [`KeyTable`], [`StatusBar`],
//! [`TearTheme`]) plus the [`MultiplexerControl`] trait that every
//! backend (in-process, local daemon, remote SSH, tmux passthrough)
//! implements.
//!
//! **No I/O. No subprocess. No filesystem.** Just types + serde.
//!
//! ## Why this crate exists at all
//!
//! `mado` (the pleme-io GPU terminal emulator) and `tear` (this
//! multiplexer) both need to model panes, windows, sessions, splits,
//! key tables, and status bars. Without a shared typed surface the
//! two apps would each grow their own duplicated `Pane` /
//! `WindowState` types — typical fleet drift. Putting the canonical
//! types here lets:
//!
//! 1. `mado` consume the same [`PaneId`] / [`WindowId`] it would
//!    receive from a remote `tear-daemon`, so tier-2 (mado driving a
//!    remote tear) and tier-3 (mado embedding `tear-core` in-process)
//!    speak the same vocabulary.
//! 2. Third-party drivers (a TUI status bar, a Slack notifier, a JIT
//!    pane-snapshot exporter) can implement [`MultiplexerControl`]
//!    against this crate alone — no runtime dep, no GPU surface, no
//!    tmux dep.
//! 3. The compounding directive's "solve once" rule applies — the
//!    next consumer (a hypothetical `gunma` shell-replay tool, a
//!    fleet pane-monitor) gets these types for free.
//!
//! ## What lives here vs. elsewhere
//!
//! Pure data + the trait surface. Anything that opens a PTY or talks
//! to disk lives in `tear-core`. Anything that opens a socket or
//! speaks RPC lives in `tear-daemon` / `tear-client`.

#![forbid(unsafe_code)]
#![doc(html_root_url = "https://docs.rs/tear-types/0.1.0")]

pub mod block;
pub mod capability;
pub mod cast;
pub mod control;
#[cfg(feature = "engate")]
pub mod engate_wrap;
pub mod direction;
pub mod geometry;
pub mod host_role;
pub mod id;
pub mod keybind;
pub mod layout;
pub mod live;
pub mod pane;
pub mod plan;
pub mod path;
pub mod pane_snapshot;
pub mod session;
pub mod spawn_env;
pub mod statusbar;
pub mod theme;
pub mod window;
pub mod wire;

pub use capability::{Capability, DaemonHello, DaemonIdentity};
pub use control::{ControlError, ControlResult, MultiplexerControl};
pub use direction::{Direction, SplitOrientation};
pub use geometry::Rect;
pub use host_role::{HostRole, TearCaps};
pub use id::{DefinitionId, InstanceId, PaneId, SessionId, WindowId};
pub use keybind::{Action, KeyBind, KeyChord, KeyTable, KeyTableName};
pub use layout::{LayoutError, LayoutKind, LayoutNode, LeafRemoval, Size, MIN_RATIO};
pub use live::{Durability, LiveSession};
pub use plan::{LayoutPlan, PaneSlot, PlanError, SpawnSpec, WindowPlan};
pub use block::Block;
pub use cast::{CastParseError, CastRow, CastRowKind};
pub use pane::{InputPolicy, PaneState, PaneStats, TearPane};
pub use pane_snapshot::{
    ansi_256_color, default_ansi_palette, Cell, CellAttrs, Color, PaneSnapshot, ANSI_BRIGHT_COLORS,
    ANSI_COLORS,
};
pub use session::{SessionSource, SessionState, TearSession};
pub use spawn_env::SpawnEnv;
pub use statusbar::{Segment, SegmentAlignment, SignalRenderMode, StatusBar, TearSignalKind};
pub use theme::HexColor;
pub use theme::TearTheme;
pub use window::{TearWindow, WindowState};
