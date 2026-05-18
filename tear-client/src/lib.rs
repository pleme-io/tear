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
use std::path::Path;

use parking_lot::Mutex;

use tear_types::wire::{read_msg, write_msg, Request, Response, WireError};
use tear_types::{
    ControlError, ControlResult, Direction, MultiplexerControl, PaneId, SessionId, TearPane,
    TearSession, TearWindow, WindowId,
};

/// A connected tear-daemon client. Implements [`MultiplexerControl`]
/// so consumer code can take `&dyn MultiplexerControl` and not care
/// whether the backend is local (`tear_core::InProcess`) or remote
/// (this `Client`).
pub struct Client {
    inner: Mutex<ClientInner>,
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
        let stream = UnixStream::connect(path)?;
        let reader_stream = stream.try_clone()?;
        Ok(Self {
            inner: Mutex::new(ClientInner {
                reader: BufReader::new(reader_stream),
                writer: BufWriter::new(stream),
            }),
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
