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

use tear_config::LiveConfig;
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
    /// Shikumi-style live config — `Arc<ArcSwap<TearConfig>>` so
    /// the daemon's request handlers can snapshot the current
    /// config at any time, and the notify watcher (held inside
    /// [`LiveConfig`]) keeps it fresh against file edits.
    config: Arc<LiveConfig>,
    /// Kept alive for the lifetime of the daemon so the notify
    /// watcher's spawned thread keeps running. Drop = stop
    /// watching.
    _config_watcher: Option<notify::RecommendedWatcher>,
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

    /// Borrow the live config — useful for tests that want to
    /// poke the config without round-tripping through the RPC,
    /// and for in-process consumers that want to subscribe to
    /// changes.
    pub fn config(&self) -> &Arc<LiveConfig> {
        &self.config
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
///
/// Loads a default-derived `LiveConfig` (reads
/// `~/.config/tear/tear.yaml` or returns default if missing) +
/// spawns the notify file watcher. Use [`start_with_config`] to
/// pass a pre-built `LiveConfig` (e.g. for tests that want to
/// poke an in-memory config without touching the filesystem).
pub fn start(socket_path: PathBuf, inproc: Arc<InProcess>) -> io::Result<DaemonHandle> {
    let live = LiveConfig::default();
    start_with_config(socket_path, inproc, Arc::new(live))
}

/// Like [`start`] but accepts an explicit `LiveConfig` so callers
/// can substitute a test-friendly config + opt in to / out of the
/// file watcher.
pub fn start_with_config(
    socket_path: PathBuf,
    inproc: Arc<InProcess>,
    live_config: Arc<LiveConfig>,
) -> io::Result<DaemonHandle> {
    if socket_path.exists() {
        let _ = std::fs::remove_file(&socket_path);
    }
    if let Some(parent) = socket_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let listener = UnixListener::bind(&socket_path)?;
    info!(path = %socket_path.display(), "tear-daemon listening");

    // Best-effort notify watcher. If spawn_watcher fails (e.g.
    // config dir doesn't exist on a brand-new fleet host) we log
    // and continue — operators can still ReloadConfig via the RPC.
    let watcher = match live_config.spawn_watcher() {
        Ok(w) => Some(w),
        Err(e) => {
            warn!(error = %e, "config file watcher could not start — manual ReloadConfig still works");
            None
        }
    };

    let stop = Arc::new(AtomicBool::new(false));
    let stop_for_accept = stop.clone();
    let inproc_for_accept = inproc.clone();
    let socket_for_accept = socket_path.clone();
    let config_for_accept = live_config.clone();

    let accept_thread = thread::Builder::new()
        .name("tear-daemon-accept".into())
        .spawn(move || {
            accept_loop(
                listener,
                stop_for_accept,
                inproc_for_accept,
                config_for_accept,
                socket_for_accept,
            )
        })?;

    Ok(DaemonHandle {
        socket_path,
        stop,
        accept_thread: Some(accept_thread),
        _inproc: inproc,
        config: live_config,
        _config_watcher: watcher,
    })
}

fn accept_loop(
    listener: UnixListener,
    stop: Arc<AtomicBool>,
    inproc: Arc<InProcess>,
    config: Arc<LiveConfig>,
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
                let config_for_conn = config.clone();
                let _ = thread::Builder::new()
                    .name("tear-daemon-conn".into())
                    .spawn(move || {
                        if let Err(e) = serve_connection(stream, inproc_for_conn, config_for_conn) {
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
    config: Arc<LiveConfig>,
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
        // SubscribeConfigChange is the same shape — promote to
        // push mode and stream Response::ConfigChanged frames.
        if matches!(req, Request::SubscribeConfigChange) {
            return serve_config_subscription(stream, config);
        }
        let resp = dispatch_with_config(&inproc, &config, req);
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

/// Push-mode handler for `Request::SubscribeConfigChange`. Writes
/// `Response::Ok` then a stream of `Response::ConfigChanged(yaml)`
/// frames every time `LiveConfig::replace` runs (notify-driven
/// reload, manual `SetConfig` RPC, or explicit `reload()`).
/// Terminates with a plain close on peer disconnect or YAML
/// serialisation failure.
fn serve_config_subscription<S: io::Read + io::Write>(
    mut stream: S,
    config: Arc<LiveConfig>,
) -> io::Result<()> {
    let rx = config.subscribe();
    write_msg(&mut stream, &Response::Ok)?;
    loop {
        match rx.recv() {
            Ok(new_cfg) => {
                let yaml = match serde_yaml_ng::to_string(&*new_cfg) {
                    Ok(y) => y,
                    Err(_) => continue, // skip un-serialisable frames
                };
                if write_msg(&mut stream, &Response::ConfigChanged(yaml)).is_err() {
                    return Ok(());
                }
            }
            // All senders dropped → LiveConfig is gone (unusual,
            // happens only at daemon shutdown). Close the stream.
            Err(_) => return Ok(()),
        }
    }
}

/// Map a Request to the corresponding InProcess call, packaging
/// the result into a Response. Pure function — no I/O — so it's
/// trivially unit-testable.
///
/// Does NOT handle the config-related requests (`GetConfig`,
/// `ReloadConfig`) because those need access to the shared
/// `LiveConfig`. Use [`dispatch_with_config`] from the serve
/// path; this entry point exists for tests that don't care about
/// config and don't want to thread an unused LiveConfig.
pub fn dispatch(inproc: &InProcess, req: Request) -> Response {
    match req {
        Request::ListSessions => map_result(inproc.list_sessions(), Response::Sessions),
        Request::GetSession(id) => map_result(inproc.get_session(id), Response::Session),
        Request::GetWindow(id) => map_result(inproc.get_window(id), |(s, w)| Response::Window {
            session: s,
            window: w,
        }),
        Request::GetPane(id) => map_result(inproc.get_pane(id), Response::Pane),
        Request::NewSession { name, shell, source } => {
            let src = source.unwrap_or_default();
            map_result(
                inproc.new_session_with_source(&name, &shell, src),
                Response::SessionId,
            )
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
        Request::SetInputPolicy { id, policy } => {
            map_unit(inproc.set_input_policy(id, policy))
        }
        Request::PaneSnapshot(id) => map_result(inproc.pane_snapshot(id), Response::PaneSnapshot),
        Request::PaneResizeAbsolute { id, cols, rows } => {
            map_unit(inproc.pane_resize_absolute(id, cols, rows))
        }
        // Subscribe is handled in serve_connection BEFORE dispatch;
        // reaching this arm means someone called dispatch directly
        // with Subscribe — programmer error.
        Request::Subscribe(_) => Response::Err(WireError::Rejected(
            "Subscribe must be handled by serve_connection (push mode), not dispatch".into(),
        )),
        // Config requests live in dispatch_with_config because they
        // need a LiveConfig handle. Same rationale as Subscribe.
        Request::GetConfig | Request::ReloadConfig | Request::SetConfig(_) => {
            Response::Err(WireError::Rejected(
                "config requests must be handled by dispatch_with_config (needs LiveConfig)"
                    .into(),
            ))
        }
        // SubscribeConfigChange is handled in serve_connection BEFORE
        // dispatch; reaching this arm means someone called dispatch
        // directly with it — programmer error.
        Request::SubscribeConfigChange => Response::Err(WireError::Rejected(
            "SubscribeConfigChange must be handled by serve_connection (push mode), not dispatch"
                .into(),
        )),
    }
}

/// Dispatcher that also handles the config RPCs. The serve loop
/// uses this; tests use the simpler `dispatch` when they don't
/// care about config.
pub fn dispatch_with_config(
    inproc: &InProcess,
    config: &LiveConfig,
    req: Request,
) -> Response {
    match req {
        Request::GetConfig => {
            let cfg = config.load();
            match serde_yaml_ng::to_string(&*cfg) {
                Ok(yaml) => Response::ConfigYaml(yaml),
                Err(e) => Response::Err(WireError::Internal(format!(
                    "failed to serialise TearConfig as YAML: {e}"
                ))),
            }
        }
        Request::ReloadConfig => match config.reload() {
            Ok(()) => Response::Ok,
            Err(e) => Response::Err(WireError::Internal(format!(
                "config reload failed: {e}"
            ))),
        },
        Request::SetConfig(yaml) => {
            match serde_yaml_ng::from_str::<tear_config::TearConfig>(&yaml) {
                Ok(cfg) => {
                    config.replace(cfg);
                    Response::Ok
                }
                Err(e) => Response::Err(WireError::Rejected(format!(
                    "SetConfig payload did not parse as TearConfig YAML: {e}"
                ))),
            }
        }
        other => dispatch(inproc, other),
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
                source: None,
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

    #[test]
    fn dispatch_subscribe_in_dispatch_path_returns_rejected() {
        // Subscribe is supposed to be intercepted in serve_connection
        // BEFORE dispatch. Calling dispatch directly with Subscribe
        // is a programmer error; verify it surfaces as a Rejected
        // response rather than blocking or panicking.
        let inproc = InProcess::new();
        let bogus = tear_types::PaneId::from_seed("nope");
        let resp = dispatch(&inproc, Request::Subscribe(bogus));
        match resp {
            Response::Err(WireError::Rejected(msg)) => {
                assert!(msg.contains("serve_connection"));
            }
            other => panic!("expected Rejected, got {other:?}"),
        }
    }

    #[test]
    fn dispatch_pane_resize_absolute_on_nonexistent_pane_returns_nosuch() {
        let inproc = InProcess::new();
        let pane = tear_types::PaneId::from_seed("nope");
        let resp = dispatch(
            &inproc,
            Request::PaneResizeAbsolute {
                id: pane,
                cols: 80,
                rows: 24,
            },
        );
        match resp {
            Response::Err(WireError::NoSuchPane(p)) => assert_eq!(p, pane),
            other => panic!("expected NoSuchPane, got {other:?}"),
        }
    }

    #[test]
    fn dispatch_pane_snapshot_on_nonexistent_pane_returns_nosuch() {
        let inproc = InProcess::new();
        let pane = tear_types::PaneId::from_seed("nope");
        let resp = dispatch(&inproc, Request::PaneSnapshot(pane));
        match resp {
            Response::Err(WireError::NoSuchPane(p)) => assert_eq!(p, pane),
            other => panic!("expected NoSuchPane, got {other:?}"),
        }
    }

    #[test]
    fn dispatch_kill_then_get_session_returns_nosuch() {
        let inproc = InProcess::new();
        // Create a session, kill it, then look it up — should error.
        let sid = match dispatch(
            &inproc,
            Request::NewSession {
                name: "x".into(),
                shell: "/bin/sh".into(),
                source: None,
            },
        ) {
            Response::SessionId(s) => s,
            other => panic!("unexpected: {other:?}"),
        };
        match dispatch(&inproc, Request::KillSession(sid)) {
            Response::Ok => {}
            other => panic!("expected Ok, got {other:?}"),
        }
        match dispatch(&inproc, Request::GetSession(sid)) {
            Response::Err(WireError::NoSuchSession(s)) => assert_eq!(s, sid),
            other => panic!("expected NoSuchSession, got {other:?}"),
        }
    }
}
