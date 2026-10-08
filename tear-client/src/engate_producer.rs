//! engate::Producer impl over a daemon-mode tear client.
//!
//! Pairs a shared `Arc<Client>` with a pane id. The Producer subscribe
//! method spawns the tear-client subscribe thread internally and
//! adapts its callback-based API to engate's `mpsc::Receiver`-based
//! contract; the Producer snapshot method does a synchronous round-trip
//! to fetch the current pane grid.
//!
//! The engate typestate contract is preserved: callers reach Live only
//! via Subscribed → Synced → Live transitions. The daemon opens every
//! subscription with the pane's history as its first `PaneBytes` frame
//! (engate M0 in tear-daemon). Against a daemon that advertises
//! `replay-modes` that frame is the attach's one replay, fenced and
//! carrying the modes, so the producer declares a stream-carried
//! history and engate takes no snapshot; against an older daemon the
//! snapshot is the replay and that frame arrives as a live item.
//!
//! See pleme-io/engate for the typestate machinery.

#![cfg(feature = "engate")]

use std::sync::Arc;
use std::sync::Mutex;
use std::sync::mpsc;

use engate_attach::{Producer, ReplaySource};
use engate_types::AttachError;
use tear_types::engate_wrap::PaneSnapshotWrap;
use tear_types::{Capability, DaemonIdentity, MultiplexerControl, PaneId};

use crate::Client;

#[must_use]
pub fn keys_read_the_mirror(replay: ReplaySource, control: &DaemonIdentity) -> bool {
    replay == ReplaySource::Stream || control.has(Capability::ReplayModes)
}

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

    fn replay_source(&self) -> ReplaySource {
        let carries = self
            .handle
            .lock()
            .unwrap()
            .as_ref()
            .is_some_and(|h| h.daemon().has(Capability::ReplayModes));
        if carries {
            ReplaySource::Stream
        } else {
            ReplaySource::Snapshot
        }
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

    #[test]
    fn the_mirror_answers_cursor_keys_unless_a_snapshot_replay_meets_a_daemon_without_replay_modes()
    {
        let new = DaemonIdentity::from_hello(tear_types::DaemonHello {
            daemon_version: "0.1.41".into(),
            capabilities: vec!["replay-modes".into()],
        });
        let old = DaemonIdentity::from_hello(tear_types::DaemonHello {
            daemon_version: "0.1.37".into(),
            capabilities: vec!["spawn-env".into()],
        });
        assert!(keys_read_the_mirror(ReplaySource::Stream, &new));
        assert!(keys_read_the_mirror(ReplaySource::Stream, &old));
        assert!(keys_read_the_mirror(ReplaySource::Snapshot, &new));
        assert!(!keys_read_the_mirror(ReplaySource::Snapshot, &old));
        assert!(!keys_read_the_mirror(
            ReplaySource::Snapshot,
            &DaemonIdentity::pre_capability()
        ));
        assert!(keys_read_the_mirror(
            ReplaySource::Snapshot,
            &DaemonIdentity::local(env!("CARGO_PKG_VERSION"))
        ));
    }

    #[test]
    fn the_replay_source_is_the_subscription_s_own_daemon_s_not_the_control_connection_s() {
        let socket =
            std::env::temp_dir().join(format!("tear-client-source-{}.sock", std::process::id()));
        let inproc = Arc::new(tear_core::InProcess::new());
        let daemon = tear_daemon::start(socket.clone(), inproc).expect("daemon");
        std::thread::sleep(Duration::from_millis(50));
        let client = Arc::new(Client::connect(&socket).expect("connect"));
        let sid = client.new_session("source", "/bin/sh").expect("session");
        let pane = *client
            .get_session(sid)
            .expect("session")
            .panes
            .keys()
            .next()
            .expect("pane");
        let producer =
            PaneProducer::new(Arc::clone(&client), pane, std::task::Waker::noop().clone());
        assert_eq!(
            producer.replay_source(),
            ReplaySource::Snapshot,
            "no subscription open, no stream to carry a history"
        );
        let _rx = producer.subscribe().expect("subscribe");
        assert_eq!(producer.replay_source(), ReplaySource::Stream);
        assert!(
            producer
                .handle
                .lock()
                .unwrap()
                .as_ref()
                .is_some_and(|h| h.daemon().has(Capability::ReplayModes)),
            "the subscription's own handshake read the daemon's capabilities"
        );
        drop(producer);
        client.kill_session(sid).expect("kill");
        daemon.stop();
    }

    #[derive(Default)]
    struct Seen {
        snapshots: usize,
        stream_replays: Vec<Vec<u8>>,
        live: usize,
    }

    struct Recording(Arc<Mutex<Seen>>);

    impl engate_attach::Consumer for Recording {
        type Item = Vec<u8>;
        type Snap = PaneSnapshotWrap;

        fn replay(&mut self, _: Self::Snap) {
            self.0.lock().unwrap().snapshots += 1;
        }

        fn consume(&mut self, _: Self::Item) {
            self.0.lock().unwrap().live += 1;
        }

        fn replay_item(&mut self, item: Self::Item) {
            self.0.lock().unwrap().stream_replays.push(item);
        }
    }

    #[test]
    fn against_a_replay_modes_daemon_the_stream_s_first_frame_is_the_one_replay() {
        let socket =
            std::env::temp_dir().join(format!("tear-client-replay-{}.sock", std::process::id()));
        let inproc = Arc::new(tear_core::InProcess::new());
        let daemon = tear_daemon::start(socket.clone(), inproc).expect("daemon");
        std::thread::sleep(Duration::from_millis(50));
        let client = Arc::new(Client::connect(&socket).expect("connect"));
        let sid = client
            .new_session_with_source_and_size(
                "replay",
                "/bin/sh",
                &[
                    "-c".to_string(),
                    "printf '\\033[?1h\\033[?2004hREADY'; exec cat".to_string(),
                ],
                tear_types::SessionSource::Human,
                (80, 24),
            )
            .expect("session");
        let pane = *client
            .get_session(sid)
            .expect("session")
            .panes
            .keys()
            .next()
            .expect("pane");
        let deadline = Instant::now() + Duration::from_secs(10);
        while !client
            .pane_snapshot(pane)
            .unwrap()
            .to_text()
            .contains("READY")
        {
            assert!(Instant::now() < deadline, "the pane never printed READY");
            std::thread::sleep(Duration::from_millis(5));
        }
        let producer =
            PaneProducer::new(Arc::clone(&client), pane, std::task::Waker::noop().clone());
        let seen = Arc::new(Mutex::new(Seen::default()));
        let (subscribed, history) = engate_attach::Attach::builder()
            .producer(producer)
            .consumer(Recording(Arc::clone(&seen)))
            .build()
            .subscribe()
            .expect("subscribe");
        assert_eq!(history.source(), ReplaySource::Stream);
        let live = subscribed.replay(history).expect("replay").start_live();
        {
            let s = seen.lock().unwrap();
            assert_eq!(
                s.snapshots, 0,
                "no snapshot RPC for a stream-carried history"
            );
            assert_eq!(s.stream_replays.len(), 1);
            let replay = String::from_utf8_lossy(&s.stream_replays[0]);
            assert!(
                replay.contains("READY")
                    && replay.contains("\x1b[?1h")
                    && replay.contains("\x1b[?2004h")
            );
        }
        drop(live);
        client.kill_session(sid).expect("kill");
        daemon.stop();
    }
}
