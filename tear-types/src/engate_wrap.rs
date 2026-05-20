//! engate Snapshot wrap for `PaneSnapshot`.
//!
//! Centralizes the wrapper so every engate Producer over a tear pane
//! (tear-core embedded, tear-client daemon, future tear-web) produces
//! the SAME `Snap` type. Consumers (mado, ayatsuri, namimado-debug)
//! impl Consumer once and ride either backend.

use engate_types::Snapshot;

use crate::pane_snapshot::PaneSnapshot;

/// Newtype wrapper carrying a `PaneSnapshot` through engate's typed
/// attach lifecycle. `to_ansi()` is the canonical serialization
/// consumers feed through their VT parser during `replay`.
pub struct PaneSnapshotWrap(pub PaneSnapshot);

impl Snapshot for PaneSnapshotWrap {
    fn size_bytes(&self) -> usize {
        // Approximate — cells × 4 bytes (char + fg/bg/attrs).
        self.0.cells.iter().map(|r| r.len() * 4).sum()
    }
}

impl PaneSnapshotWrap {
    /// Borrow ANSI replay bytes — same wire-shape the daemon's
    /// engate M0 path emits as the first PaneBytes frame.
    #[must_use]
    pub fn to_ansi(&self) -> Vec<u8> {
        self.0.to_ansi()
    }

    #[must_use]
    pub fn into_inner(self) -> PaneSnapshot {
        self.0
    }
}

impl From<PaneSnapshot> for PaneSnapshotWrap {
    fn from(s: PaneSnapshot) -> Self {
        Self(s)
    }
}
