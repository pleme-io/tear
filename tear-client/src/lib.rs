//! `tear-client` — typed RPC client for `tear-daemon`.
//!
//! Connects over UDS locally, over SSH/mosh-tunneled UDS remotely.
//! Speaks the same typed `MultiplexerControl` trait the daemon
//! implements — connection mode is the *only* difference visible to
//! the consumer.
//!
//! ## Shape
//!
//! [`Client`] holds a `parking_lot::Mutex<UnixStream>`. Each
//! `MultiplexerControl` call:
//!
//! 1. Acquires the mutex.
//! 2. Writes a length-prefixed bincode [`Request`].
//! 3. Reads a length-prefixed bincode [`Response`].
//! 4. Decodes the response variant into the trait's return type.
//!
//! The mutex serialises requests within one `Client`. Multiple
//! `Client`s connected to the same daemon get their own connections
//! and proceed in parallel — that's how mado will scale across
//! Tier-2/Tier-3 callers without head-of-line blocking.
//!
//! ## Why sync
//!
//! The trait is sync. The PTY pump is on the daemon side, not the
//! client side. Async at this layer would be all cost no benefit —
//! the client's job is to ferry a few Request/Response pairs per
//! human keystroke, not pump kilobytes per second.

#![forbid(unsafe_code)]

use std::io::{self, BufReader, BufWriter};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;

use parking_lot::Mutex;

use tear_types::wire::{read_msg, write_msg, Request, Response};
use tear_types::{
    ControlError, ControlResult, Direction, MultiplexerControl, PaneId, PaneSnapshot, SessionId,
    TearPane, TearSession, TearWindow, WindowId,
};

/// A connected tear-daemon client. Implements [`MultiplexerControl`]
/// so consumer code can take `&dyn MultiplexerControl` and not care
/// whether the backend is local (`tear_core::InProcess`) or remote
/// (this `Client`).
pub struct Client {
    inner: Mutex<ClientInner>,
    /// Path the client connected to. Subscriptions need to dial
    /// the same daemon on a fresh socket because Subscribe consumes
    /// the connection.
    socket_path: PathBuf,
}

/// Handle returned by [`Client::subscribe_pane_bytes`]. Dropping
/// it disconnects the subscription connection (the daemon's serve
/// thread observes the write error on the next chunk and prunes
/// the dead sender).
pub struct SubscribeHandle {
    stop: Arc<AtomicBool>,
    join: Option<thread::JoinHandle<()>>,
}

impl SubscribeHandle {
    /// Signal the reader thread to stop and join it. Idempotent —
    /// safe to call multiple times.
    pub fn stop(mut self) {
        self.signal_and_join();
    }

    fn signal_and_join(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(j) = self.join.take() {
            let _ = j.join();
        }
    }
}

impl Drop for SubscribeHandle {
    fn drop(&mut self) {
        if self.join.is_some() {
            self.signal_and_join();
        }
    }
}

/// The buffered halves of the UDS stream. Buffered so the framed
/// reads/writes don't translate into a syscall per byte.
struct ClientInner {
    reader: BufReader<UnixStream>,
    writer: BufWriter<UnixStream>,
}

impl Client {
    /// Connect to a tear-daemon listening at `path`.
    ///
    /// Returns the `io::Error` from the underlying `UnixStream::connect`
    /// unchanged so callers can distinguish "no daemon there"
    /// (`NotFound`) from "permission" (`PermissionDenied`) etc.
    pub fn connect(path: impl AsRef<Path>) -> io::Result<Self> {
        let path_buf = path.as_ref().to_path_buf();
        let stream = UnixStream::connect(&path_buf)?;
        let reader_stream = stream.try_clone()?;
        Ok(Self {
            inner: Mutex::new(ClientInner {
                reader: BufReader::new(reader_stream),
                writer: BufWriter::new(stream),
            }),
            socket_path: path_buf,
        })
    }

    /// Path the client is connected to. Mostly useful for the
    /// subscribe API which needs to open a second connection to
    /// the same daemon.
    pub fn socket_path(&self) -> &Path {
        &self.socket_path
    }

    /// Subscribe to a pane's PTY byte stream. Opens a fresh UDS
    /// connection to the same daemon, sends `Request::Subscribe`,
    /// then spawns a reader thread that calls `on_bytes` for every
    /// `Response::PaneBytes` frame. The reader exits on
    /// `Response::PaneClosed`, on EOF, or when the returned
    /// [`SubscribeHandle`] is dropped / stopped.
    ///
    /// `on_bytes` runs on the reader thread — keep it cheap and
    /// non-blocking. Typical consumer: push the bytes into a
    /// channel for the render loop to drain.
    pub fn subscribe_pane_bytes<F>(
        &self,
        pane: PaneId,
        mut on_bytes: F,
    ) -> ControlResult<SubscribeHandle>
    where
        F: FnMut(&[u8]) + Send + 'static,
    {
        // Subscriptions ride a separate connection because they
        // consume the stream — the control connection has to stay
        // free for further RPCs.
        let stream = UnixStream::connect(&self.socket_path)
            .map_err(|e| ControlError::Transport(e.to_string()))?;
        let reader_stream = stream
            .try_clone()
            .map_err(|e| ControlError::Transport(e.to_string()))?;
        let mut reader = BufReader::new(reader_stream);
        let mut writer = BufWriter::new(stream);
        write_msg(&mut writer, &Request::Subscribe(pane))
            .map_err(|e| ControlError::Transport(e.to_string()))?;
        // First reply: Ok or Err (NoSuchPane / etc.).
        let ack: Response = read_msg(&mut reader)
            .map_err(|e| ControlError::Transport(e.to_string()))?;
        match ack {
            Response::Ok => {}
            Response::Err(we) => return Err(ControlError::from(we)),
            other => {
                return Err(ControlError::Transport(format!(
                    "unexpected ack to Subscribe: {other:?}"
                )))
            }
        }
        let stop = Arc::new(AtomicBool::new(false));
        let stop_for_thread = stop.clone();
        let join = thread::Builder::new()
            .name("tear-client-subscribe".into())
            .spawn(move || {
                while !stop_for_thread.load(Ordering::SeqCst) {
                    match read_msg::<_, Response>(&mut reader) {
                        Ok(Response::PaneBytes(b)) => on_bytes(&b),
                        Ok(Response::PaneClosed(_)) => return,
                        Ok(_) => return, // unexpected variant — bail
                        Err(_) => return, // EOF / I/O error → done
                    }
                }
            })
            .map_err(|e| ControlError::Transport(format!("spawn subscriber thread: {e}")))?;
        Ok(SubscribeHandle {
            stop,
            join: Some(join),
        })
    }

    /// Connect to the default socket location ([`tear_types::wire::default_socket_path`]).
    pub fn connect_default() -> io::Result<Self> {
        Self::connect(tear_types::wire::default_socket_path())
    }

    /// Send one Request, read one Response. Mutexed so a single
    /// `Client` is internally serialised; the wire format is
    /// request-response per the daemon's contract.
    fn rpc(&self, req: Request) -> ControlResult<Response> {
        let mut inner = self.inner.lock();
        let ClientInner { reader, writer } = &mut *inner;
        write_msg::<_, Request>(writer, &req)
            .map_err(|e| ControlError::Transport(e.to_string()))?;
        let resp: Response = read_msg(reader)
            .map_err(|e| ControlError::Transport(e.to_string()))?;
        if let Response::Err(we) = resp {
            return Err(ControlError::from(we));
        }
        Ok(resp)
    }
}

impl MultiplexerControl for Client {
    fn list_sessions(&self) -> ControlResult<Vec<TearSession>> {
        match self.rpc(Request::ListSessions)? {
            Response::Sessions(v) => Ok(v),
            other => Err(unexpected("Sessions", other)),
        }
    }

    fn get_session(&self, id: SessionId) -> ControlResult<TearSession> {
        match self.rpc(Request::GetSession(id))? {
            Response::Session(s) => Ok(s),
            other => Err(unexpected("Session", other)),
        }
    }

    fn get_window(&self, id: WindowId) -> ControlResult<(SessionId, TearWindow)> {
        match self.rpc(Request::GetWindow(id))? {
            Response::Window { session, window } => Ok((session, window)),
            other => Err(unexpected("Window", other)),
        }
    }

    fn get_pane(&self, id: PaneId) -> ControlResult<TearPane> {
        match self.rpc(Request::GetPane(id))? {
            Response::Pane(p) => Ok(p),
            other => Err(unexpected("Pane", other)),
        }
    }

    fn new_session(&self, name: &str, shell: &str) -> ControlResult<SessionId> {
        match self.rpc(Request::NewSession {
            name: name.to_owned(),
            shell: shell.to_owned(),
        })? {
            Response::SessionId(id) => Ok(id),
            other => Err(unexpected("SessionId", other)),
        }
    }

    fn rename_session(&self, id: SessionId, new_name: &str) -> ControlResult<()> {
        match self.rpc(Request::RenameSession {
            id,
            new_name: new_name.to_owned(),
        })? {
            Response::Ok => Ok(()),
            other => Err(unexpected("Ok", other)),
        }
    }

    fn kill_session(&self, id: SessionId) -> ControlResult<()> {
        match self.rpc(Request::KillSession(id))? {
            Response::Ok => Ok(()),
            other => Err(unexpected("Ok", other)),
        }
    }

    fn new_window(
        &self,
        session: SessionId,
        name: &str,
        shell: &str,
    ) -> ControlResult<WindowId> {
        match self.rpc(Request::NewWindow {
            session,
            name: name.to_owned(),
            shell: shell.to_owned(),
        })? {
            Response::WindowId(id) => Ok(id),
            other => Err(unexpected("WindowId", other)),
        }
    }

    fn kill_window(&self, id: WindowId) -> ControlResult<()> {
        match self.rpc(Request::KillWindow(id))? {
            Response::Ok => Ok(()),
            other => Err(unexpected("Ok", other)),
        }
    }

    fn select_window(&self, id: WindowId) -> ControlResult<()> {
        match self.rpc(Request::SelectWindow(id))? {
            Response::Ok => Ok(()),
            other => Err(unexpected("Ok", other)),
        }
    }

    fn split_pane(
        &self,
        origin: PaneId,
        direction: Direction,
        shell: &str,
    ) -> ControlResult<PaneId> {
        match self.rpc(Request::SplitPane {
            origin,
            direction,
            shell: shell.to_owned(),
        })? {
            Response::PaneId(id) => Ok(id),
            other => Err(unexpected("PaneId", other)),
        }
    }

    fn kill_pane(&self, id: PaneId) -> ControlResult<()> {
        match self.rpc(Request::KillPane(id))? {
            Response::Ok => Ok(()),
            other => Err(unexpected("Ok", other)),
        }
    }

    fn select_pane(&self, id: PaneId) -> ControlResult<()> {
        match self.rpc(Request::SelectPane(id))? {
            Response::Ok => Ok(()),
            other => Err(unexpected("Ok", other)),
        }
    }

    fn resize_pane(
        &self,
        id: PaneId,
        direction: Direction,
        delta_cells: i16,
    ) -> ControlResult<()> {
        match self.rpc(Request::ResizePane {
            id,
            direction,
            delta_cells,
        })? {
            Response::Ok => Ok(()),
            other => Err(unexpected("Ok", other)),
        }
    }

    fn send_keys(&self, id: PaneId, bytes: &[u8]) -> ControlResult<()> {
        match self.rpc(Request::SendKeys {
            id,
            bytes: bytes.to_vec(),
        })? {
            Response::Ok => Ok(()),
            other => Err(unexpected("Ok", other)),
        }
    }

    fn pane_snapshot(&self, id: PaneId) -> ControlResult<PaneSnapshot> {
        match self.rpc(Request::PaneSnapshot(id))? {
            Response::PaneSnapshot(snap) => Ok(snap),
            other => Err(unexpected("PaneSnapshot", other)),
        }
    }

    fn pane_resize_absolute(&self, id: PaneId, cols: u16, rows: u16) -> ControlResult<()> {
        match self.rpc(Request::PaneResizeAbsolute { id, cols, rows })? {
            Response::Ok => Ok(()),
            other => Err(unexpected("Ok", other)),
        }
    }
}

/// Daemon broke the contract — wrong response variant for a given
/// Request. Surfaces as a `Transport` error so callers handle it
/// the same way as a network glitch (retry once + give up).
fn unexpected(want: &'static str, got: Response) -> ControlError {
    ControlError::Transport(format!(
        "tear-daemon returned wrong response variant: expected {want}, got {got:?}"
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    /// End-to-end round-trip: spin up an in-process daemon, connect
    /// a client to its UDS, drive it through several
    /// MultiplexerControl ops, confirm the daemon-side state matches.
    #[test]
    fn client_drives_daemon_round_trip() {
        let socket = {
            let mut p = std::env::temp_dir();
            let pid = std::process::id();
            p.push(format!("tear-client-test-{pid}.sock"));
            p
        };
        let inproc = Arc::new(tear_core::InProcess::new());
        let daemon =
            tear_daemon::start(socket.clone(), inproc.clone()).expect("daemon should start");

        // Give the accept thread a beat to bind.
        std::thread::sleep(std::time::Duration::from_millis(50));

        let client = Client::connect(&socket).expect("client connect");

        // 1. Fresh daemon — list is empty.
        let initial = client.list_sessions().unwrap();
        assert!(initial.is_empty(), "fresh daemon should have 0 sessions");

        // 2. Create a session.
        let sid = client.new_session("work", "/bin/sh").unwrap();

        // 3. List sees it.
        let listed = client.list_sessions().unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].id, sid);

        // 4. get_session returns it.
        let got = client.get_session(sid).unwrap();
        assert_eq!(got.id, sid);
        assert_eq!(got.name, "work");

        // 5. Rename + reread.
        client.rename_session(sid, "play").unwrap();
        let renamed = client.get_session(sid).unwrap();
        assert_eq!(renamed.name, "play");

        // 6. Error path: get a nonexistent session, expect NoSuchSession.
        let bogus = SessionId::from_seed("bogus");
        match client.get_session(bogus) {
            Err(ControlError::NoSuchSession(id)) => assert_eq!(id, bogus),
            other => panic!("expected NoSuchSession, got {other:?}"),
        }

        // 7. Daemon-side state matches client view.
        let daemon_list = daemon.inproc().list_sessions().unwrap();
        assert_eq!(daemon_list.len(), 1);
        assert_eq!(daemon_list[0].id, sid);

        drop(client);
        daemon.stop();
    }

    /// **Phase 2 end-to-end**: spin up an in-process daemon, create
    /// a session (which spawns `/bin/sh` in a real PTY), send a
    /// known command, poll `pane_snapshot` over the wire, and
    /// assert the command's output text appears in the snapshot.
    ///
    /// Proves the full vertical: send_keys → kernel PTY → child
    /// shell → child stdout → PTY master read thread →
    /// `PaneGrid::feed` → vte parser → snapshot → CBOR serialize →
    /// UDS → CBOR deserialize → client returns text.
    #[test]
    fn end_to_end_send_keys_then_pane_snapshot_shows_output() {
        let socket = {
            let mut p = std::env::temp_dir();
            let pid = std::process::id();
            p.push(format!("tear-client-e2e-{pid}.sock"));
            p
        };
        let inproc = Arc::new(tear_core::InProcess::new());
        let daemon =
            tear_daemon::start(socket.clone(), inproc.clone()).expect("daemon start");
        std::thread::sleep(std::time::Duration::from_millis(50));

        let client = Client::connect(&socket).expect("connect");

        // /bin/sh keeps the test portable across macOS / Linux /
        // NixOS — every host has it on PATH at this canonical
        // location. Bash / zsh would also work but are not
        // guaranteed everywhere.
        let sid = client
            .new_session("phase2-e2e", "/bin/sh")
            .expect("new_session");
        let session = client.get_session(sid).expect("get_session");

        // The single first pane is the one we want.
        let pane_id = *session
            .panes
            .keys()
            .next()
            .expect("session should have one pane");

        // sh starts up + prints a prompt; nudge it with a known
        // command whose output we can grep for. Use a unique marker
        // so any pre-existing prompt text doesn't false-positive.
        let marker = "MADO_TEAR_E2E_MARK_8421";
        let cmd = format!("printf '{marker}\\n'\n");
        client
            .send_keys(pane_id, cmd.as_bytes())
            .expect("send_keys");

        // Poll up to 2s for the marker to appear in the snapshot —
        // PTY round-trip latency varies on busy CI hardware.
        let deadline = std::time::Instant::now()
            + std::time::Duration::from_secs(2);
        let mut got = String::new();
        while std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(50));
            match client.pane_snapshot(pane_id) {
                Ok(snap) => {
                    got = snap.to_text();
                    if got.contains(marker) {
                        break;
                    }
                }
                Err(e) => panic!("pane_snapshot failed: {e}"),
            }
        }
        assert!(
            got.contains(marker),
            "marker `{marker}` never appeared in snapshot.\nGot:\n{got}"
        );

        drop(client);
        daemon.stop();
    }

    /// **Phase 2.5 push subscription end-to-end**: subscribe to a
    /// pane's byte stream, send a marker, verify the subscriber
    /// thread receives the bytes.
    #[test]
    fn end_to_end_subscribe_pane_bytes_pushes_pty_output() {
        let socket = {
            let mut p = std::env::temp_dir();
            let pid = std::process::id();
            p.push(format!("tear-client-sub-{pid}.sock"));
            p
        };
        let inproc = Arc::new(tear_core::InProcess::new());
        let daemon = tear_daemon::start(socket.clone(), inproc).expect("daemon");
        std::thread::sleep(std::time::Duration::from_millis(50));

        let client = Client::connect(&socket).expect("connect");
        let sid = client.new_session("sub-e2e", "/bin/sh").unwrap();
        let session = client.get_session(sid).unwrap();
        let pane_id = *session.panes.keys().next().expect("pane");

        // Channel for the subscriber thread to publish bytes back.
        let (tx, rx) = std::sync::mpsc::channel::<Vec<u8>>();
        let handle = client
            .subscribe_pane_bytes(pane_id, move |bytes| {
                let _ = tx.send(bytes.to_vec());
            })
            .expect("subscribe");

        let marker = "TEAR_SUBSCRIBE_MARK_5821";
        client
            .send_keys(pane_id, format!("printf '{marker}\\n'\n").as_bytes())
            .expect("send_keys");

        // Drain the channel for up to 2s looking for the marker.
        let deadline = std::time::Instant::now()
            + std::time::Duration::from_secs(2);
        let mut accum = Vec::new();
        while std::time::Instant::now() < deadline {
            if let Ok(chunk) = rx.recv_timeout(std::time::Duration::from_millis(100)) {
                accum.extend(chunk);
                if std::str::from_utf8(&accum)
                    .map(|s| s.contains(marker))
                    .unwrap_or(false)
                {
                    break;
                }
            }
        }
        let text = String::from_utf8_lossy(&accum).to_string();
        assert!(
            text.contains(marker),
            "subscriber never received marker `{marker}`. Accumulated bytes:\n{text}"
        );

        handle.stop();
        drop(client);
        daemon.stop();
    }

    /// Two concurrent subscribers to the same pane each receive the
    /// full byte stream — the daemon fans out on every PTY chunk,
    /// not just to the first registrant.
    #[test]
    fn two_subscribers_each_receive_pane_bytes() {
        let socket = {
            let mut p = std::env::temp_dir();
            let pid = std::process::id();
            p.push(format!("tear-client-fanout-{pid}.sock"));
            p
        };
        let inproc = Arc::new(tear_core::InProcess::new());
        let daemon = tear_daemon::start(socket.clone(), inproc).expect("daemon");
        std::thread::sleep(std::time::Duration::from_millis(50));

        let client = Client::connect(&socket).expect("connect");
        let sid = client.new_session("fanout", "/bin/sh").unwrap();
        let pane_id = *client
            .get_session(sid)
            .unwrap()
            .panes
            .keys()
            .next()
            .unwrap();

        let (tx_a, rx_a) = std::sync::mpsc::channel::<Vec<u8>>();
        let (tx_b, rx_b) = std::sync::mpsc::channel::<Vec<u8>>();
        let _h_a = client
            .subscribe_pane_bytes(pane_id, move |b| {
                let _ = tx_a.send(b.to_vec());
            })
            .expect("sub A");
        let _h_b = client
            .subscribe_pane_bytes(pane_id, move |b| {
                let _ = tx_b.send(b.to_vec());
            })
            .expect("sub B");

        let marker = "FANOUT_DUAL_MARK_3902";
        client
            .send_keys(pane_id, format!("printf '{marker}\\n'\n").as_bytes())
            .unwrap();

        // Each subscriber should accumulate the marker independently.
        let collect = |rx: &std::sync::mpsc::Receiver<Vec<u8>>| {
            let mut acc = Vec::new();
            let deadline = std::time::Instant::now()
                + std::time::Duration::from_secs(2);
            while std::time::Instant::now() < deadline {
                if let Ok(c) = rx.recv_timeout(std::time::Duration::from_millis(100)) {
                    acc.extend(c);
                    if std::str::from_utf8(&acc)
                        .map(|s| s.contains(marker))
                        .unwrap_or(false)
                    {
                        break;
                    }
                }
            }
            acc
        };
        let got_a = collect(&rx_a);
        let got_b = collect(&rx_b);
        let text_a = String::from_utf8_lossy(&got_a).to_string();
        let text_b = String::from_utf8_lossy(&got_b).to_string();
        assert!(text_a.contains(marker), "subscriber A missed marker:\n{text_a}");
        assert!(text_b.contains(marker), "subscriber B missed marker:\n{text_b}");
        daemon.stop();
    }

    /// pane_resize_absolute via the wire flips snapshot dimensions
    /// on the next snapshot, AND survives a follow-up subscribe.
    #[test]
    fn pane_resize_absolute_propagates_to_snapshot() {
        let socket = {
            let mut p = std::env::temp_dir();
            let pid = std::process::id();
            p.push(format!("tear-client-resize-{pid}.sock"));
            p
        };
        let inproc = Arc::new(tear_core::InProcess::new());
        let daemon = tear_daemon::start(socket.clone(), inproc).expect("daemon");
        std::thread::sleep(std::time::Duration::from_millis(50));
        let client = Client::connect(&socket).expect("connect");
        let sid = client.new_session("resize", "/bin/sh").unwrap();
        let pane_id = *client
            .get_session(sid)
            .unwrap()
            .panes
            .keys()
            .next()
            .unwrap();

        // Default is 80x24 — confirm.
        let initial = client.pane_snapshot(pane_id).unwrap();
        assert_eq!(initial.cols, 80);
        assert_eq!(initial.rows, 24);

        // Resize to 120x40.
        client.pane_resize_absolute(pane_id, 120, 40).unwrap();
        let after = client.pane_snapshot(pane_id).unwrap();
        assert_eq!(after.cols, 120);
        assert_eq!(after.rows, 40);
        daemon.stop();
    }

    /// Even when the daemon has been stopped, a fresh connect
    /// attempt fails with `NotFound` (no leftover socket) — proves
    /// the cleanup-on-drop story.
    #[test]
    fn connect_after_daemon_stop_returns_not_found() {
        let socket = {
            let mut p = std::env::temp_dir();
            let pid = std::process::id();
            p.push(format!("tear-client-stop-test-{pid}.sock"));
            p
        };
        let inproc = Arc::new(tear_core::InProcess::new());
        let daemon = tear_daemon::start(socket.clone(), inproc).expect("daemon start");
        std::thread::sleep(std::time::Duration::from_millis(50));
        daemon.stop();
        std::thread::sleep(std::time::Duration::from_millis(50));
        match Client::connect(&socket) {
            Ok(_) => panic!("expected NotFound, daemon should be gone"),
            Err(e) => assert_eq!(e.kind(), io::ErrorKind::NotFound),
        }
    }
}
