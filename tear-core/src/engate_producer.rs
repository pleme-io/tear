//! engate::Producer impl for an `InProcess` pane.
//!
//! Wraps the in-process `InProcess::pane_snapshot` + `subscribe_pane_bytes`
//! pair into the engate typed-attach contract. With this impl, a mado
//! GUI process can:
//!
//! 1. Embed `InProcess` directly (no daemon hop, no IPC).
//! 2. Construct an `engate::Attach<Live>` via `Attach::builder()` —
//!    the same call site the daemon-mode path uses.
//! 3. Get the engate typestate guarantees (history-replayed-before-
//!    render, drop-bomb on forgotten History, etc.) for free.
//!
//! Embedding does NOT remove the second VT parse. The pane's
//! `PaneGrid` parses every byte in the PTY callback
//! (`InProcess::pane_callbacks`, `grid.feed`), and the consumer parses
//! the same bytes again from `subscribe_pane_bytes` — two parses per
//! output byte in embedded mode as in daemon mode (PERFORMANCE.md §2
//! C1, §3 class 9). Embedded saves the socket hop, not the parse;
//! the single parse is R38's flip, where the consumer reads the
//! authority's view instead of re-parsing its bytes.

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
/// represents, and the waker every chunk and the stream's end ring.
pub struct PaneProducer {
    pub inproc: Arc<InProcess>,
    pub pane: PaneId,
    waker: std::task::Waker,
}

impl PaneProducer {
    #[must_use]
    pub fn new(inproc: Arc<InProcess>, pane: PaneId, waker: std::task::Waker) -> Self {
        Self {
            inproc,
            pane,
            waker,
        }
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
            .subscribe_pane_bytes_waking(self.pane, self.waker.clone())
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
        let producer = PaneProducer::new(inproc, pane, std::task::Waker::noop().clone());
        let snap = producer.snapshot().expect("snapshot");
        assert_eq!(snap.0.cols, 80);
        assert_eq!(snap.0.rows, 24);
        let _rx = producer.subscribe().expect("subscribe");
    }

    struct Rings(std::sync::Mutex<usize>, std::sync::Condvar);

    impl std::task::Wake for Rings {
        fn wake(self: Arc<Self>) {
            *self.0.lock().unwrap() += 1;
            self.1.notify_all();
        }
    }

    impl Rings {
        fn at_least(&self, n: usize) -> usize {
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
            let mut got = self.0.lock().unwrap();
            while *got < n {
                let left = deadline.saturating_duration_since(std::time::Instant::now());
                if left.is_zero() {
                    break;
                }
                got = self.1.wait_timeout(got, left).unwrap().0;
            }
            *got
        }
    }

    #[test]
    fn a_chunk_rings_the_producer_s_waker_and_so_does_the_end_of_the_stream() {
        let inproc = Arc::new(InProcess::new());
        let sid = inproc
            .new_session_with_source_and_size(
                "engate-wake",
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
        let rings = Arc::new(Rings(std::sync::Mutex::new(0), std::sync::Condvar::new()));
        let producer = PaneProducer::new(
            Arc::clone(&inproc),
            pane,
            std::task::Waker::from(Arc::clone(&rings)),
        );
        let rx = producer.subscribe().expect("subscribe");
        inproc.send_keys(pane, b"echo ring\n").expect("type");
        assert!(rings.at_least(1) >= 1, "output rang the waker");
        let chunk = rx
            .recv_timeout(std::time::Duration::from_secs(10))
            .expect("the ring's chunk is queued");
        assert!(!chunk.is_empty());
        while rx.try_recv().is_ok() {}
        let before = *rings.0.lock().unwrap();
        inproc.kill_session(sid).expect("kill");
        assert!(
            rings.at_least(before + 1) > before,
            "the end rang the waker"
        );
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        let closed = loop {
            match rx.try_recv() {
                Err(std::sync::mpsc::TryRecvError::Disconnected) => break true,
                _ if std::time::Instant::now() > deadline => break false,
                Ok(_) => {}
                Err(std::sync::mpsc::TryRecvError::Empty) => {
                    std::thread::sleep(std::time::Duration::from_millis(5));
                }
            }
        };
        assert!(closed, "the kill closed the stream");
    }
}
