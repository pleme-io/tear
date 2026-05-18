//! `tear-daemon` — long-running tear server.
//!
//! Owns sessions across client disconnects; exposes typed UDS RPC so
//! `tear-client` (or any consumer — mado at Tier 2, fleet operators
//! over SSH at Tier 3) can drive the [`tear_types::MultiplexerControl`]
//! surface remotely.
//!
//! Wraps `tear_core::InProcess` rather than reimplementing — pane
//! semantics live in one place fleet-wide.
//!
//! ## Architecture
//!
//! A single `UnixListener` accepts connections. Each connection gets
//! its own OS thread that runs [`serve_connection`] — a synchronous
//! loop reading [`Request`] frames, dispatching to the shared
//! `Arc<InProcess>`, and writing [`Response`] frames back. The
//! per-connection thread approach matches the trait's sync surface
//! and avoids async-runtime overhead the daemon doesn't need —
//! `InProcess` already uses `parking_lot` for the registry lock, so
//! reads scale across threads naturally.
//!
//! ## Lifecycle
//!
//! [`start`] is the canonical entry point: it binds the socket,
//! creates the InProcess instance, spawns the accept loop, and
//! returns a [`DaemonHandle`] the caller can use to wait or stop.
//! Dropping the handle stops accepting new connections and joins
//! the accept thread; existing connections continue until the
//! peer closes them.

#![forbid(unsafe_code)]

use std::io;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;

use tear_core::InProcess;
use tear_types::wire::{read_msg, write_msg, Request, Response, WireError};
use tear_types::MultiplexerControl;
use tracing::{debug, error, info, warn};

/// Handle returned by [`start`]. Dropping it (or calling
/// [`DaemonHandle::stop`]) stops the accept loop and joins the
/// accept thread.
pub struct DaemonHandle {
    socket_path: PathBuf,
    stop: Arc<AtomicBool>,
    accept_thread: Option<thread::JoinHandle<()>>,
    /// Kept alive so dropping the handle drops the InProcess (and
    /// every PTY it owns) only when the operator decides to.
    _inproc: Arc<InProcess>,
}

impl DaemonHandle {
    /// Path of the UDS the daemon is listening on. Same value the
    /// caller passed to [`start`]; clients dial this.
    pub fn socket_path(&self) -> &Path {
        &self.socket_path
    }

    /// Borrow the shared `InProcess` — handy for tests that want to
    /// inspect daemon-side state without going through the RPC.
    pub fn inproc(&self) -> &Arc<InProcess> {
        &self._inproc
    }

    /// Signal the accept loop to exit, then join it. Existing
    /// connections continue. The socket file is unlinked.
    pub fn stop(mut self) {
        self.signal_and_join();
    }

    fn signal_and_join(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        // Nudge the blocking accept() — a self-connect is the
        // cheapest cross-platform way to wake it; the accept
        // returns, sees stop=true, exits.
        let _ = UnixStream::connect(&self.socket_path);
        if let Some(h) = self.accept_thread.take() {
            let _ = h.join();
        }
        let _ = std::fs::remove_file(&self.socket_path);
    }
}

impl Drop for DaemonHandle {
    fn drop(&mut self) {
        if self.accept_thread.is_some() {
            self.signal_and_join();
        }
    }
}

/// Bind a UDS at `socket_path` and start serving requests. If a
/// stale socket file exists at the path it's unlinked first — the
/// daemon assumes single-instance semantics; ship a `tear daemon
/// --no-replace` if that ever becomes wrong.
pub fn start(socket_path: PathBuf, inproc: Arc<InProcess>) -> io::Result<DaemonHandle> {
    if socket_path.exists() {
        let _ = std::fs::remove_file(&socket_path);
    }
    if let Some(parent) = socket_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let listener = UnixListener::bind(&socket_path)?;
    info!(path = %socket_path.display(), "tear-daemon listening");

    let stop = Arc::new(AtomicBool::new(false));
    let stop_for_accept = stop.clone();
    let inproc_for_accept = inproc.clone();
    let socket_for_accept = socket_path.clone();

    let accept_thread = thread::Builder::new()
        .name("tear-daemon-accept".into())
        .spawn(move || accept_loop(listener, stop_for_accept, inproc_for_accept, socket_for_accept))?;

    Ok(DaemonHandle {
        socket_path,
        stop,
        accept_thread: Some(accept_thread),
        _inproc: inproc,
    })
}

fn accept_loop(
    listener: UnixListener,
    stop: Arc<AtomicBool>,
    inproc: Arc<InProcess>,
    _socket_path: PathBuf,
) {
    for incoming in listener.incoming() {
        if stop.load(Ordering::SeqCst) {
            debug!("accept loop: stop requested");
            return;
        }
        match incoming {
            Ok(stream) => {
                let inproc_for_conn = inproc.clone();
                let _ = thread::Builder::new()
                    .name("tear-daemon-conn".into())
                    .spawn(move || {
                        if let Err(e) = serve_connection(stream, inproc_for_conn) {
                            if e.kind() != io::ErrorKind::UnexpectedEof {
                                warn!(error = %e, "connection ended");
                            }
                        }
                    });
            }
            Err(e) => {
                error!(error = %e, "accept failed");
            }
        }
    }
}

/// Serve a single client connection. Each request is read in full,
/// dispatched synchronously against the shared `InProcess`, and
/// the response is written before the next request is read — no
/// pipelining at this layer.
///
/// `Request::Subscribe` is the one special case: the connection is
/// promoted to push-mode after the initial `Response::Ok` and
/// streams `Response::PaneBytes` frames until the pane closes or
/// the peer disconnects. No further Requests are read from a
/// subscribed connection.
///
/// This function is public so tests (and embedded consumers that
/// want to hand-stitch transports beyond UDS) can drive it
/// directly with their own `Read + Write` stream.
pub fn serve_connection<S: io::Read + io::Write>(
    mut stream: S,
    inproc: Arc<InProcess>,
) -> io::Result<()> {
    loop {
        let req: Request = match read_msg(&mut stream) {
            Ok(r) => r,
            Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => return Ok(()),
            Err(e) => return Err(e),
        };
        // Subscribe promotes the connection to push mode.
        if let Request::Subscribe(pane) = req {
            return serve_subscription(stream, inproc, pane);
        }
        let resp = dispatch(&inproc, req);
        write_msg(&mut stream, &resp)?;
    }
}

/// Push-mode handler invoked after a `Request::Subscribe`. Writes
/// `Response::Ok` then a stream of `Response::PaneBytes` frames as
/// the pane's PTY produces output. Terminates with
/// `Response::PaneClosed` when the pane is killed (or with a plain
/// close on write error / peer disconnect).
fn serve_subscription<S: io::Read + io::Write>(
    mut stream: S,
    inproc: Arc<InProcess>,
    pane: tear_types::PaneId,
) -> io::Result<()> {
    // Register the subscriber. On NoSuchPane we still respond with
    // Err so the client knows immediately + closes the connection.
    let rx = match inproc.subscribe_pane_bytes(pane) {
        Ok(rx) => rx,
        Err(e) => {
            write_msg(&mut stream, &Response::Err(WireError::from(e)))?;
            return Ok(());
        }
    };
    write_msg(&mut stream, &Response::Ok)?;
    // Drain the receiver synchronously — recv blocks until either a
    // chunk arrives or every sender has been dropped (pane killed).
    loop {
        match rx.recv() {
            Ok(bytes) => {
                if write_msg(&mut stream, &Response::PaneBytes(bytes)).is_err() {
                    // Peer disconnected — drop our sender by
                    // letting the rx drop naturally. The next PTY
                    // chunk on InProcess prunes the dead sender.
                    return Ok(());
                }
            }
            Err(_) => {
                // All senders dropped → the pane is gone.
                let _ = write_msg(&mut stream, &Response::PaneClosed(pane));
                return Ok(());
            }
        }
    }
}

/// Map a Request to the corresponding InProcess call, packaging
/// the result into a Response. Pure function — no I/O — so it's
/// trivially unit-testable.
pub fn dispatch(inproc: &InProcess, req: Request) -> Response {
    match req {
        Request::ListSessions => map_result(inproc.list_sessions(), Response::Sessions),
        Request::GetSession(id) => map_result(inproc.get_session(id), Response::Session),
        Request::GetWindow(id) => map_result(inproc.get_window(id), |(s, w)| Response::Window {
            session: s,
            window: w,
        }),
        Request::GetPane(id) => map_result(inproc.get_pane(id), Response::Pane),
        Request::NewSession { name, shell } => {
            map_result(inproc.new_session(&name, &shell), Response::SessionId)
        }
        Request::RenameSession { id, new_name } => {
            map_unit(inproc.rename_session(id, &new_name))
        }
        Request::KillSession(id) => map_unit(inproc.kill_session(id)),
        Request::NewWindow { session, name, shell } => {
            map_result(inproc.new_window(session, &name, &shell), Response::WindowId)
        }
        Request::KillWindow(id) => map_unit(inproc.kill_window(id)),
        Request::SelectWindow(id) => map_unit(inproc.select_window(id)),
        Request::SplitPane { origin, direction, shell } => {
            map_result(inproc.split_pane(origin, direction, &shell), Response::PaneId)
        }
        Request::KillPane(id) => map_unit(inproc.kill_pane(id)),
        Request::SelectPane(id) => map_unit(inproc.select_pane(id)),
        Request::ResizePane { id, direction, delta_cells } => {
            map_unit(inproc.resize_pane(id, direction, delta_cells))
        }
        Request::SendKeys { id, bytes } => map_unit(inproc.send_keys(id, &bytes)),
        Request::PaneSnapshot(id) => map_result(inproc.pane_snapshot(id), Response::PaneSnapshot),
        // Subscribe is handled in serve_connection BEFORE dispatch;
        // reaching this arm means someone called dispatch directly
        // with Subscribe — programmer error.
        Request::Subscribe(_) => Response::Err(WireError::Rejected(
            "Subscribe must be handled by serve_connection (push mode), not dispatch".into(),
        )),
    }
}

fn map_result<T, F: FnOnce(T) -> Response>(
    r: tear_types::ControlResult<T>,
    ok: F,
) -> Response {
    match r {
        Ok(v) => ok(v),
        Err(e) => Response::Err(WireError::from(e)),
    }
}

fn map_unit(r: tear_types::ControlResult<()>) -> Response {
    match r {
        Ok(()) => Response::Ok,
        Err(e) => Response::Err(WireError::from(e)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[test]
    fn dispatch_list_sessions_on_fresh_inproc_returns_empty() {
        let inproc = InProcess::new();
        let resp = dispatch(&inproc, Request::ListSessions);
        match resp {
            Response::Sessions(v) => assert!(v.is_empty()),
            other => panic!("unexpected response: {other:?}"),
        }
    }

    #[test]
    fn dispatch_new_session_then_list_sees_it() {
        let inproc = InProcess::new();
        let resp = dispatch(
            &inproc,
            Request::NewSession {
                name: "work".into(),
                shell: "/bin/sh".into(),
            },
        );
        let session_id = match resp {
            Response::SessionId(id) => id,
            other => panic!("unexpected response: {other:?}"),
        };
        let listed = dispatch(&inproc, Request::ListSessions);
        match listed {
            Response::Sessions(v) => {
                assert_eq!(v.len(), 1);
                assert_eq!(v[0].id, session_id);
            }
            other => panic!("unexpected response: {other:?}"),
        }
    }

    #[test]
    fn dispatch_get_nonexistent_session_returns_wire_error() {
        let inproc = InProcess::new();
        let bogus = tear_types::SessionId::from_seed("nope");
        let resp = dispatch(&inproc, Request::GetSession(bogus));
        match resp {
            Response::Err(WireError::NoSuchSession(id)) => assert_eq!(id, bogus),
            other => panic!("unexpected response: {other:?}"),
        }
    }

    #[test]
    fn round_trip_via_in_memory_buffer() {
        let inproc = Arc::new(InProcess::new());
        // Encode a request into a buffer.
        let mut buf = Vec::new();
        write_msg(&mut buf, &Request::ListSessions).unwrap();
        let mut cur = Cursor::new(buf);
        let req: Request = read_msg(&mut cur).unwrap();
        // Dispatch and encode the response.
        let resp = dispatch(&inproc, req);
        let mut out = Vec::new();
        write_msg(&mut out, &resp).unwrap();
        let mut out_cur = Cursor::new(out);
        let got: Response = read_msg(&mut out_cur).unwrap();
        assert!(matches!(got, Response::Sessions(v) if v.is_empty()));
    }
}
