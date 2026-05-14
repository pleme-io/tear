//! The [`MultiplexerControl`] trait — every tear backend implements it.
//!
//! - `tear-core::InProcess` — embedded state machine. The native
//!   backend used when `tear` runs without a daemon, and the backend
//!   mado will eventually compose with directly (M5 — see project
//!   plan in CLAUDE.md).
//! - `tear-daemon` — owns sessions across client disconnects, exposes
//!   a UDS RPC façade. Wraps `InProcess` rather than reimplementing.
//! - `tear-client` — typed RPC client connected to a local or
//!   SSH-tunnelled `tear-daemon`. Implements `MultiplexerControl` so
//!   from the consumer's perspective there's no syntactic difference
//!   between local and remote.
//! - `tear-tmux-backend` — passthrough to vanilla tmux. The escape
//!   hatch for remote hosts that have tmux but not tear.
//!
//! All four backends share this trait so consumers (the CLI, mado,
//! third-party drivers) author against ONE surface.

use thiserror::Error;

use crate::{
    direction::Direction,
    id::{PaneId, SessionId, WindowId},
    pane::TearPane,
    session::TearSession,
    window::TearWindow,
};

/// Result alias for `MultiplexerControl` operations.
pub type ControlResult<T> = Result<T, ControlError>;

/// Failure modes a [`MultiplexerControl`] op can return. Variants
/// stay narrow so consumers can pattern-match on the recoverable
/// vs not-recoverable ones (e.g. RPC retries on `Transport` but
/// gives up on `NoSuchSession`).
#[derive(Debug, Error)]
pub enum ControlError {
    #[error("no such session: {0}")]
    NoSuchSession(SessionId),
    #[error("no such window: {0}")]
    NoSuchWindow(WindowId),
    #[error("no such pane: {0}")]
    NoSuchPane(PaneId),
    #[error("backend transport error: {0}")]
    Transport(String),
    #[error("backend rejected operation: {0}")]
    Rejected(String),
    #[error("backend internal: {0}")]
    Internal(#[from] anyhow::Error),
}

/// The trait every tear backend implements. Operations are
/// intentionally **async-free** at the trait level — backends that
/// want async runtimes spin them up internally; consumers (CLI,
/// mado) call these directly on a worker thread.
///
/// Designed for the COMMON case (one session, a few windows, a few
/// panes). Backends that need to batch operations (e.g. tear-daemon
/// over a slow link) wrap individual calls themselves.
pub trait MultiplexerControl: Send + Sync {
    // ── Discovery ────────────────────────────────────────────────

    /// List every active session. Sorted by creation time (oldest
    /// first); the CLI's `tear list` renders this directly.
    fn list_sessions(&self) -> ControlResult<Vec<TearSession>>;

    /// Look up a single session by id.
    fn get_session(&self, id: SessionId) -> ControlResult<TearSession>;

    /// Look up a single window. Returns the window + its parent
    /// session id (handy for rendering window paths like `work:1`).
    fn get_window(&self, id: WindowId) -> ControlResult<(SessionId, TearWindow)>;

    /// Look up a single pane.
    fn get_pane(&self, id: PaneId) -> ControlResult<TearPane>;

    // ── Sessions ─────────────────────────────────────────────────

    /// Create a new session. `name` is the operator-visible label;
    /// the returned [`SessionId`] is the stable handle.
    fn new_session(&self, name: &str, shell: &str) -> ControlResult<SessionId>;

    /// Rename a session. Idempotent — renaming to the current name
    /// returns Ok(()) without side effects.
    fn rename_session(&self, id: SessionId, new_name: &str) -> ControlResult<()>;

    /// Kill a session and all its children.
    fn kill_session(&self, id: SessionId) -> ControlResult<()>;

    // ── Windows ──────────────────────────────────────────────────

    /// Create a new window in a session, spawning `shell` as its
    /// first pane.
    fn new_window(&self, session: SessionId, name: &str, shell: &str) -> ControlResult<WindowId>;

    fn kill_window(&self, id: WindowId) -> ControlResult<()>;

    fn select_window(&self, id: WindowId) -> ControlResult<()>;

    // ── Panes ────────────────────────────────────────────────────

    /// Split a pane in the given direction. `shell` is the program
    /// spawned in the new pane.
    fn split_pane(
        &self,
        origin: PaneId,
        direction: Direction,
        shell: &str,
    ) -> ControlResult<PaneId>;

    fn kill_pane(&self, id: PaneId) -> ControlResult<()>;

    fn select_pane(&self, id: PaneId) -> ControlResult<()>;

    /// Resize a pane along one axis. `delta_cells` is signed — negative
    /// shrinks. tmux's `resize-pane -L/-R/-U/-D`.
    fn resize_pane(&self, id: PaneId, direction: Direction, delta_cells: i16) -> ControlResult<()>;

    /// Send keystrokes (already-bytes-encoded — caller resolved the
    /// chord) to a pane's PTY.
    fn send_keys(&self, id: PaneId, bytes: &[u8]) -> ControlResult<()>;
}
