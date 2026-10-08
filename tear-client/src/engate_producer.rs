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
use engate_types::AttachError;
use tear_types::engate_wrap::PaneSnapshotWrap;
use tear_types::{MultiplexerControl, PaneId};

use crate::Client;

/// engate Producer over a daemon-mode tear client.
pub struct PaneProducer {
    pub client: Arc<Client>,
    pub pane: PaneId,
    /// Subscribe returns an mpsc::Receiver; the SubscribeHandle that
    /// owns the daemon connection lives here so the subscription
    /// outlasts the call.
    handle: Mutex<Option<crate::SubscribeHandle>>,
    waker: std::task::Waker,
}

impl PaneProducer {
    #[must_use]
    pub fn new(client: Arc<Client>, pane: PaneId, waker: std::task::Waker) -> Self {
        Self {
            client,
            pane,
            handle: Mutex::new(None),
            waker,
        }
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
        let (tx, rx) = tear_types::waking::WakingSender::channel(self.waker.clone());
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Condvar;
    use std::time::{Duration, Instant};

    struct Rings(Mutex<usize>, Condvar);

    impl std::task::Wake for Rings {
        fn wake(self: Arc<Self>) {
            *self.0.lock().unwrap() += 1;
            self.1.notify_all();
        }
    }

    impl Rings {
        fn count(&self) -> usize {
            *self.0.lock().unwrap()
        }

        fn at_least(&self, n: usize) -> usize {
            let deadline = Instant::now() + Duration::from_secs(10);
            let mut got = self.0.lock().unwrap();
            while *got < n {
                let left = deadline.saturating_duration_since(Instant::now());
                if left.is_zero() {
                    break;
                }
                got = self.1.wait_timeout(got, left).unwrap().0;
            }
            *got
        }
    }

    #[test]
    fn a_chunk_rings_the_producer_s_waker_and_so_does_pane_closed() {
        let socket =
            std::env::temp_dir().join(format!("tear-client-wake-{}.sock", std::process::id()));
        let inproc = Arc::new(tear_core::InProcess::new());
        let daemon = tear_daemon::start(socket.clone(), inproc).expect("daemon");
        std::thread::sleep(Duration::from_millis(50));
        let client = Arc::new(Client::connect(&socket).expect("connect"));
        let sid = client.new_session("wake", "/bin/sh").expect("session");
        let pane = *client
            .get_session(sid)
            .expect("session")
            .panes
            .keys()
            .next()
            .expect("pane");
        let rings = Arc::new(Rings(Mutex::new(0), Condvar::new()));
        let producer = PaneProducer::new(
            Arc::clone(&client),
            pane,
            std::task::Waker::from(Arc::clone(&rings)),
        );
        let rx = producer.subscribe().expect("subscribe");
        client.send_keys(pane, b"echo ring\n").expect("type");
        assert!(rings.at_least(1) >= 1, "a chunk rang the waker");
        assert!(rx.recv_timeout(Duration::from_secs(10)).is_ok());
        while rx.try_recv().is_ok() {}
        let before = rings.count();
        client.kill_session(sid).expect("kill");
        assert!(
            rings.at_least(before + 1) > before,
            "PaneClosed ended the stream and rang the waker"
        );
        let end = Instant::now() + Duration::from_secs(10);
        let closed = loop {
            match rx.try_recv() {
                Err(mpsc::TryRecvError::Disconnected) => break true,
                _ if Instant::now() > end => break false,
                _ => std::thread::sleep(Duration::from_millis(5)),
            }
        };
        assert!(closed, "the ring for the end found the stream closed");
        drop(producer);
        daemon.stop();
    }
}
