//! engate::Producer impl over a daemon-mode tear client.
//!
//! Pairs a shared `Arc<Client>` with a pane id. The Producer subscribe
//! method spawns the tear-client subscribe thread internally and
//! adapts its callback-based API to engate's `mpsc::Receiver`-based
//! contract; the Producer snapshot method does a synchronous round-trip
//! to fetch the current pane grid.
//!
//! The engate typestate contract is preserved: callers reach Live only
//! via Subscribed → Synced → Live transitions. The wire-level history
//! bytes are also still delivered as the first PaneBytes frame
//! (engate M0 in tear-daemon), so a misbehaving producer is caught
//! TWICE — once by engate's typestate, once by the daemon's protocol.
//!
//! See pleme-io/engate for the typestate machinery.

#![cfg(feature = "engate")]

use std::sync::Arc;
use std::sync::Mutex;
use std::sync::mpsc;

use engate_attach::Producer;
use engate_types::{AttachError, Snapshot};
use tear_types::{MultiplexerControl, PaneId, PaneSnapshot};

use crate::Client;

/// engate Producer over a daemon-mode tear client.
pub struct PaneProducer {
    pub client: Arc<Client>,
    pub pane: PaneId,
    /// Subscribe returns an mpsc::Receiver; the SubscribeHandle that
    /// owns the daemon connection lives here so the subscription
    /// outlasts the call.
    handle: Mutex<Option<crate::SubscribeHandle>>,
}

impl PaneProducer {
    #[must_use]
    pub fn new(client: Arc<Client>, pane: PaneId) -> Self {
        Self {
            client,
            pane,
            handle: Mutex::new(None),
        }
    }
}

/// Wrapper so we can implement engate's Snapshot trait on the
/// foreign PaneSnapshot type.
pub struct PaneSnapshotWrap(pub PaneSnapshot);

impl Snapshot for PaneSnapshotWrap {
    fn size_bytes(&self) -> usize {
        self.0.cells.iter().map(|r| r.len() * 4).sum()
    }
}

impl PaneSnapshotWrap {
    #[must_use]
    pub fn to_ansi(&self) -> Vec<u8> {
        self.0.to_ansi()
    }

    #[must_use]
    pub fn into_inner(self) -> PaneSnapshot {
        self.0
    }
}

impl Producer for PaneProducer {
    type Item = Vec<u8>;
    type Snap = PaneSnapshotWrap;

    fn snapshot(&self) -> Result<Self::Snap, AttachError> {
        self.client
            .pane_snapshot(self.pane)
            .map(PaneSnapshotWrap)
            .map_err(|e| AttachError::SnapshotFailed(e.to_string()))
    }

    fn subscribe(&self) -> Result<mpsc::Receiver<Self::Item>, AttachError> {
        let (tx, rx) = mpsc::channel();
        let h = self
            .client
            .subscribe_pane_bytes(self.pane, move |bytes| {
                let _ = tx.send(bytes.to_vec());
            })
            .map_err(|e| AttachError::SubscribeFailed(e.to_string()))?;
        // Keep the handle alive — dropping it closes the subscribe
        // connection.
        *self.handle.lock().unwrap() = Some(h);
        Ok(rx)
    }
}
