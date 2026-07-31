//! engate::Producer impl for an `InProcess` pane.
//!
//! Wraps the in-process `InProcess::pane_snapshot` + `subscribe_pane_bytes`
//! pair into the engate typed-attach contract. With this impl, a mado
//! GUI process can:
//!
//! 1. Embed `InProcess` directly (no daemon hop, no IPC, no second
//!    VT parse).
//! 2. Construct an `engate::Attach<Live>` via `Attach::builder()` —
//!    the same call site the daemon-mode path uses.
//! 3. Get the engate typestate guarantees (history-replayed-before-
//!    render, drop-bomb on forgotten History, etc.) for free.
//!
//! The latency math: embedded mode = ghostty-class single-process
//! PTY-to-pixel; daemon mode = the same typed contract through a
//! Unix socket. The composition stays correct; the operator picks
//! the latency/multi-attach tradeoff via maestro's StackSpec.

#![cfg(feature = "engate")]

use std::sync::Arc;
use std::sync::mpsc;

use engate_attach::{Consumer, Producer};
use engate_types::AttachError;
use tear_types::PaneId;
use tear_types::engate_wrap::PaneSnapshotWrap;

use crate::inproc::InProcess;

/// engate Producer over an InProcess pane.
///
/// Pairs a shared `InProcess` handle with the pane id this producer
/// represents. Cheap to clone; the `Arc<InProcess>` is the only
/// shared state.
pub struct PaneProducer {
    pub inproc: Arc<InProcess>,
    pub pane: PaneId,
}

impl PaneProducer {
    #[must_use]
    pub fn new(inproc: Arc<InProcess>, pane: PaneId) -> Self {
        Self { inproc, pane }
    }
}

impl Producer for PaneProducer {
    type Item = Vec<u8>;
    type Snap = PaneSnapshotWrap;

    fn snapshot(&self) -> Result<Self::Snap, AttachError> {
        // engate contract: subscribe FIRST, snapshot SECOND so no
        // item in the window is lost. Caller (Attach::subscribe)
        // already invokes subscribe before snapshot, so we just
        // capture here.
        self.inproc
            .pane_snapshot(self.pane)
            .map(PaneSnapshotWrap)
            .map_err(|e| AttachError::SnapshotFailed(e.to_string()))
    }

    fn subscribe(&self) -> Result<mpsc::Receiver<Self::Item>, AttachError> {
        self.inproc
            .subscribe_pane_bytes(self.pane)
            .map_err(|e| AttachError::SubscribeFailed(e.to_string()))
    }
}

/// Helper for consumers whose `replay` impl wants to feed the ANSI
/// replay bytes through their existing VT parser. The default Consumer
/// impl in mado is exactly this shape.
pub fn replay_via_consume<C>(consumer: &mut C, snap: PaneSnapshotWrap)
where
    C: Consumer<Item = Vec<u8>, Snap = PaneSnapshotWrap>,
{
    let bytes = snap.to_ansi();
    consumer.consume(bytes);
}

#[cfg(test)]
mod tests {
    use super::*;
    use tear_types::{MultiplexerControl, SessionSource};

    #[test]
    fn pane_producer_snapshot_and_subscribe() {
        let inproc = Arc::new(InProcess::new());
        let sid = inproc
            .new_session_with_source_and_size(
                "engate-test",
                "/bin/sh",
                &[],
                SessionSource::Human,
                (80, 24),
            )
            .expect("spawn session");
        let pane = inproc.with_registry(|r| {
            r.sessions
                .get(&sid)
                .and_then(|s| s.panes.keys().next().copied())
                .expect("pane")
        });
        let producer = PaneProducer::new(inproc, pane);
        let snap = producer.snapshot().expect("snapshot");
        assert_eq!(snap.0.cols, 80);
        assert_eq!(snap.0.rows, 24);
        let _rx = producer.subscribe().expect("subscribe");
    }
}
