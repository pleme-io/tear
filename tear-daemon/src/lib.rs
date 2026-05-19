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

pub mod audit;
#[cfg(any(test, feature = "testing"))]
pub mod testing;

use std::io;
use std::net::{SocketAddr, TcpListener};
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

use crate::audit::{AuditEvent, AuditLog};

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
    /// #6 — append-only audit log handle, opened from the
    /// initial config's `audit_log` field. `None` when no
    /// audit_log was configured. Hot-reload-of-audit-path is
    /// deliberately out of scope (operator restarts the daemon
    /// to switch sinks).
    audit: Option<AuditLog>,
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
/// #5 — start a TCP-bound tear-daemon. Same wire (CBOR) as the
/// UDS variant; serve_connection is already generic over `Read +
/// Write`. For untrusted networks tunnel through SSH or run
/// behind a TLS proxy — this layer is unencrypted.
pub fn start_tcp(addr: SocketAddr, inproc: Arc<InProcess>) -> io::Result<DaemonHandle> {
    let live = LiveConfig::default();
    start_tcp_with_config(addr, inproc, Arc::new(live))
}

/// #5 — like [`start_tcp`] but with an explicit `LiveConfig`.
pub fn start_tcp_with_config(
    addr: SocketAddr,
    inproc: Arc<InProcess>,
    live_config: Arc<LiveConfig>,
) -> io::Result<DaemonHandle> {
    let listener = TcpListener::bind(addr)?;
    let bound = listener.local_addr()?;
    info!(addr = %bound, "tear-daemon listening (tcp)");

    let watcher = match live_config.spawn_watcher() {
        Ok(w) => Some(w),
        Err(e) => {
            warn!(error = %e, "config file watcher could not start (tcp daemon)");
            None
        }
    };

    let audit = open_audit_from_config(&live_config);
    let required_token = resolve_required_token(&live_config)?;

    let stop = Arc::new(AtomicBool::new(false));
    let stop_for_accept = stop.clone();
    let inproc_for_accept = inproc.clone();
    let config_for_accept = live_config.clone();
    let audit_for_accept = audit.clone();
    let token_for_accept = required_token.clone();

    let accept_thread = thread::Builder::new()
        .name("tear-daemon-accept-tcp".into())
        .spawn(move || {
            accept_loop_tcp(
                listener,
                stop_for_accept,
                inproc_for_accept,
                config_for_accept,
                audit_for_accept,
                token_for_accept,
            )
        })?;

    // Use a synthetic socket_path so DaemonHandle.signal_and_join's
    // UDS-self-connect nudge is a no-op for TCP. The accept loop
    // checks `stop` on a short timeout via set_nonblocking, so the
    // wake-up doesn't need to be filesystem-mediated.
    Ok(DaemonHandle {
        socket_path: PathBuf::from(format!("tcp://{bound}")),
        stop,
        accept_thread: Some(accept_thread),
        _inproc: inproc,
        config: live_config,
        audit,
        _config_watcher: watcher,
    })
}

/// Open the audit log from the current config snapshot. Returns
/// None when no `audit_log` is set or the file couldn't be
/// opened (logged at warn level — audit failures are
/// best-effort, never block startup).
fn open_audit_from_config(live: &LiveConfig) -> Option<AuditLog> {
    let path = live.load().audit_log.clone()?;
    match AuditLog::open(&path) {
        Ok(a) => Some(a),
        Err(e) => {
            warn!(path, error = %e, "audit: open failed; audit disabled this run");
            None
        }
    }
}

/// #5 — resolve the required auth token from the env var named in
/// the live config. Returns Some(token) only when the config sets
/// `auth_token_env` AND the env var is non-empty. A configured env
/// var that is missing or empty fails startup loudly (operator
/// misconfiguration) by returning an io::Error from the caller.
fn resolve_required_token(live: &LiveConfig) -> io::Result<Option<String>> {
    let Some(name) = live.load().auth_token_env.clone() else {
        return Ok(None);
    };
    match std::env::var(&name) {
        Ok(v) if !v.is_empty() => {
            info!(env_var = %name, "auth: requiring token on every client connection");
            Ok(Some(v))
        }
        Ok(_) => Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!("auth_token_env={name} is set but the env var is empty"),
        )),
        Err(_) => Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!(
                "auth_token_env={name} is set but the env var is not present; \
                 export it before starting `tear daemon`"
            ),
        )),
    }
}

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

    // Stamp the bound socket path on the InProcess backend so
    // every PTY child it spawns can inherit `TEAR_SOCKET=<path>`
    // (alongside TEAR_SESSION_ID/NAME and TEAR_PANE_ID) — shells
    // and starship rely on it for prompt visibility + re-discovery.
    inproc.set_socket_path(socket_path.clone());

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

    let audit = open_audit_from_config(&live_config);
    let required_token = resolve_required_token(&live_config)?;

    let stop = Arc::new(AtomicBool::new(false));
    let stop_for_accept = stop.clone();
    let inproc_for_accept = inproc.clone();
    let socket_for_accept = socket_path.clone();
    let config_for_accept = live_config.clone();
    let audit_for_accept = audit.clone();
    let token_for_accept = required_token.clone();

    let accept_thread = thread::Builder::new()
        .name("tear-daemon-accept".into())
        .spawn(move || {
            accept_loop(
                listener,
                stop_for_accept,
                inproc_for_accept,
                config_for_accept,
                socket_for_accept,
                audit_for_accept,
                token_for_accept,
            )
        })?;

    Ok(DaemonHandle {
        socket_path,
        stop,
        accept_thread: Some(accept_thread),
        _inproc: inproc,
        config: live_config,
        audit,
        _config_watcher: watcher,
    })
}

/// TCP accept loop — mirrors `accept_loop` but reads from a
/// TcpListener. Uses `set_nonblocking` + a short sleep so the
/// stop flag can fire promptly (no UDS-self-connect trick for
/// TCP).
fn accept_loop_tcp(
    listener: TcpListener,
    stop: Arc<AtomicBool>,
    inproc: Arc<InProcess>,
    config: Arc<LiveConfig>,
    audit: Option<AuditLog>,
    required_token: Option<String>,
) {
    listener
        .set_nonblocking(true)
        .expect("set_nonblocking on TcpListener");
    loop {
        if stop.load(Ordering::SeqCst) {
            debug!("accept loop (tcp): stop requested");
            return;
        }
        match listener.accept() {
            Ok((stream, peer)) => {
                debug!(peer = %peer, "tcp connection accepted");
                let inproc_for_conn = inproc.clone();
                let config_for_conn = config.clone();
                let audit_for_conn = audit.clone();
                let token_for_conn = required_token.clone();
                let _ = thread::Builder::new()
                    .name("tear-daemon-conn-tcp".into())
                    .spawn(move || {
                        // Per-connection: switch back to blocking
                        // mode (set_nonblocking on TcpListener is
                        // inherited by accepted TcpStream).
                        let _ = stream.set_nonblocking(false);
                        if let Err(e) = serve_connection_with_auth(
                            stream,
                            inproc_for_conn,
                            config_for_conn,
                            audit_for_conn,
                            token_for_conn,
                        ) {
                            if e.kind() != io::ErrorKind::UnexpectedEof {
                                warn!(error = %e, "tcp connection ended");
                            }
                        }
                    });
            }
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
            Err(e) => {
                error!(error = %e, "tcp accept failed");
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
        }
    }
}

fn accept_loop(
    listener: UnixListener,
    stop: Arc<AtomicBool>,
    inproc: Arc<InProcess>,
    config: Arc<LiveConfig>,
    _socket_path: PathBuf,
    audit: Option<AuditLog>,
    required_token: Option<String>,
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
                let audit_for_conn = audit.clone();
                let token_for_conn = required_token.clone();
                let _ = thread::Builder::new()
                    .name("tear-daemon-conn".into())
                    .spawn(move || {
                        if let Err(e) = serve_connection_with_auth(
                            stream,
                            inproc_for_conn,
                            config_for_conn,
                            audit_for_conn,
                            token_for_conn,
                        ) {
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
    stream: S,
    inproc: Arc<InProcess>,
    config: Arc<LiveConfig>,
    audit: Option<AuditLog>,
) -> io::Result<()> {
    serve_connection_with_auth(stream, inproc, config, audit, None)
}

/// #5 — like `serve_connection` but enforces an optional
/// shared-secret auth token. When `required_token` is `Some`, the
/// connection rejects every request with `WireError::Rejected(...)`
/// until it receives a matching `Request::Authenticate(token)`.
/// When `None`, behaves exactly like `serve_connection`. Sending
/// Authenticate to an unauth'd-required daemon is silently `Ok`
/// (forward-compatible with clients that pre-emptively authenticate).
pub fn serve_connection_with_auth<S: io::Read + io::Write>(
    mut stream: S,
    inproc: Arc<InProcess>,
    config: Arc<LiveConfig>,
    audit: Option<AuditLog>,
    required_token: Option<String>,
) -> io::Result<()> {
    let mut authed = required_token.is_none();
    // #2 — per-connection client identity. Set by IdentifyClient;
    // read on SendKeys to enforce Leader policy.
    let mut client_id: Option<u64> = None;
    loop {
        let req: Request = match read_msg(&mut stream) {
            Ok(r) => r,
            Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => return Ok(()),
            Err(e) => return Err(e),
        };
        // Handle Authenticate first so a pre-emptive token works
        // on either kind of daemon. Constant-time compare to avoid
        // a timing oracle (every byte compared regardless of mismatch
        // position).
        if let Request::Authenticate(presented) = &req {
            let resp = match &required_token {
                Some(expected) if ct_eq(expected.as_bytes(), presented.as_bytes()) => {
                    authed = true;
                    Response::Ok
                }
                Some(_) => Response::Err(tear_types::wire::WireError::Rejected(
                    "auth failed".into(),
                )),
                None => Response::Ok,
            };
            write_msg(&mut stream, &resp)?;
            continue;
        }
        if !authed {
            let resp = Response::Err(tear_types::wire::WireError::Rejected(
                "authentication required: send Authenticate(token) first".into(),
            ));
            write_msg(&mut stream, &resp)?;
            continue;
        }
        // #2 — IdentifyClient sets the per-connection client_id and
        // returns Ok. No-op for daemons whose panes are all Free/Locked.
        if let Request::IdentifyClient(id) = &req {
            client_id = Some(*id);
            write_msg(&mut stream, &Response::Ok)?;
            continue;
        }
        // #2 — Leader-policy gate. Before forwarding SendKeys to the
        // InProcess (whose own `Locked` check still runs after), peek
        // at the pane's input_policy. If it's `Leader(want)` and the
        // connection's client_id doesn't match, reject locally.
        if let Request::SendKeys { id, .. } = &req {
            if let Ok(pane) = inproc.get_pane(*id) {
                if let Some(want) = pane.input_policy.leader_id() {
                    if client_id != Some(want) {
                        let resp = Response::Err(tear_types::wire::WireError::Rejected(
                            format!(
                                "leader policy: pane {id:?} requires client_id={want}, \
                                 connection identified as {client_id:?}"
                            ),
                        ));
                        write_msg(&mut stream, &resp)?;
                        continue;
                    }
                }
            }
        }
        // Subscribe promotes the connection to push mode.
        if let Request::Subscribe(pane) = req {
            return serve_subscription(stream, inproc, pane);
        }
        // SubscribeConfigChange is the same shape — promote to
        // push mode and stream Response::ConfigChanged frames.
        if matches!(req, Request::SubscribeConfigChange) {
            return serve_config_subscription(stream, config);
        }
        let resp = dispatch_with_config(&inproc, &config, req, audit.as_ref());
        write_msg(&mut stream, &resp)?;
    }
}

/// Constant-time byte-slice equality. Returns false for differing
/// lengths without comparing further; otherwise compares every byte
/// and folds. Cheap and adequate for short shared-secret tokens.
fn ct_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff: u8 = 0;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
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
        Request::PaneSubscriberCount(id) => {
            map_result(inproc.pane_subscriber_count(id), Response::SubscriberCount)
        }
        // #4 — recording RPCs route directly to the InProcess
        // inherent methods (not part of MultiplexerControl since
        // recording is a tear-core-side primitive, not a generic
        // multiplexer concept).
        Request::StartPaneRecording(id) => map_unit(inproc.enable_pane_recording(id)),
        Request::StopPaneRecording(id) => map_unit(inproc.disable_pane_recording(id)),
        Request::ExportPaneRecording(id) => {
            map_result(inproc.export_pane_recording(id), Response::CastJson)
        }
        Request::PaneRecordingStatus(id) => {
            map_result(inproc.pane_recording_status(id), |(enabled, events)| {
                Response::RecordingStatus { enabled, events }
            })
        }
        Request::PaneBlocksList { pane, since_index, limit } => {
            map_result(inproc.pane_blocks_list(pane, since_index, limit), Response::Blocks)
        }
        Request::PaneBlockAt { pane, index } => {
            map_result(inproc.pane_block_at(pane, index), Response::Block)
        }
        Request::PaneBlocksStatus(id) => {
            map_result(inproc.pane_blocks_status(id), |(total, in_progress)| {
                Response::BlocksStatus { total, in_progress }
            })
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
        // #5 — Authenticate is intercepted in serve_connection_with_auth
        // before dispatch ever sees it. Reaching this arm means a
        // test (or in-process consumer) called dispatch directly with
        // Authenticate — accept it as a no-op so test ergonomics
        // don't suffer.
        Request::Authenticate(_) => Response::Ok,
        // #2 — IdentifyClient is intercepted in serve_connection_with_auth
        // (it sets per-connection state). Reaching dispatch directly is
        // a test-only path; accept as Ok so tests can construct request
        // streams that include IdentifyClient frames.
        Request::IdentifyClient(_) => Response::Ok,
    }
}

/// Dispatcher that also handles the config RPCs. The serve loop
/// uses this; tests use the simpler `dispatch` when they don't
/// care about config.
pub fn dispatch_with_config(
    inproc: &InProcess,
    config: &LiveConfig,
    req: Request,
    audit: Option<&AuditLog>,
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
                    let hash = hash_str(&yaml);
                    config.replace(cfg);
                    if let Some(a) = audit {
                        a.emit(&AuditEvent::SetConfig {
                            ts_ms: AuditEvent::now_ms(),
                            config_hash: hash,
                        });
                    }
                    Response::Ok
                }
                Err(e) => Response::Err(WireError::Rejected(format!(
                    "SetConfig payload did not parse as TearConfig YAML: {e}"
                ))),
            }
        }
        // #48c — auto-flush any active recordings BEFORE the kill
        // wipes the session. Reads `recording_auto_dir` from the
        // live config; if unset, behaves identically to the pre-#48c
        // path. Errors are logged but never block the kill — the
        // operator's "stop this thing now" intent always wins.
        Request::KillSession(id) => {
            if let Some(dir) = config.load().recording_auto_dir.clone() {
                flush_session_recordings(inproc, id, &dir);
            }
            let resp = map_unit(inproc.kill_session(id));
            if matches!(resp, Response::Ok) {
                if let Some(a) = audit {
                    a.emit(&AuditEvent::SessionKill {
                        ts_ms: AuditEvent::now_ms(),
                        sid: id.to_string(),
                    });
                }
            }
            resp
        }
        Request::NewSession { name, shell, source } => {
            let src = source.unwrap_or_default();
            let result = inproc.new_session_with_source(&name, &shell, src.clone());
            if let Ok(sid) = &result {
                if let Some(a) = audit {
                    a.emit(&AuditEvent::SessionCreate {
                        ts_ms: AuditEvent::now_ms(),
                        sid: sid.to_string(),
                        name: name.clone(),
                        shell: shell.clone(),
                        source: src.label().to_string(),
                    });
                }
            }
            map_result(result, Response::SessionId)
        }
        Request::SetInputPolicy { id, policy } => {
            let resp = map_unit(inproc.set_input_policy(id, policy));
            if matches!(resp, Response::Ok) {
                if let Some(a) = audit {
                    a.emit(&AuditEvent::SetInputPolicy {
                        ts_ms: AuditEvent::now_ms(),
                        pid: id.to_string(),
                        policy: policy.label().to_string(),
                    });
                }
            }
            resp
        }
        Request::StartPaneRecording(id) => {
            let resp = map_unit(inproc.enable_pane_recording(id));
            if matches!(resp, Response::Ok) {
                if let Some(a) = audit {
                    a.emit(&AuditEvent::StartRecording {
                        ts_ms: AuditEvent::now_ms(),
                        pid: id.to_string(),
                    });
                }
            }
            resp
        }
        Request::StopPaneRecording(id) => {
            let resp = map_unit(inproc.disable_pane_recording(id));
            if matches!(resp, Response::Ok) {
                if let Some(a) = audit {
                    a.emit(&AuditEvent::StopRecording {
                        ts_ms: AuditEvent::now_ms(),
                        pid: id.to_string(),
                    });
                }
            }
            resp
        }
        other => dispatch(inproc, other),
    }
}

/// Tiny hex SHA-256 helper — used to fingerprint SetConfig
/// payloads for the audit log without bloating each row.
fn hash_str(s: &str) -> String {
    // tear already pulls in blake3 for InProcess id minting; reuse.
    let hash = blake3::hash(s.as_bytes());
    hash.to_hex().to_string()
}

/// Best-effort: walk every pane in the session, export each
/// pane's recording (if any), write to
/// `<dir>/<session_id>-<unix_ts>-<pane_id>.cast`. Skip panes
/// without a recording (export returns empty / NoSuchPane).
fn flush_session_recordings(inproc: &InProcess, session: tear_types::SessionId, dir: &str) {
    use std::time::{SystemTime, UNIX_EPOCH};
    let expanded = tear_types::path::expand_tilde(dir);
    if std::fs::create_dir_all(&expanded).is_err() {
        warn!(dir = %expanded, "auto-flush: mkdir failed; skipping");
        return;
    }
    let panes: Vec<tear_types::PaneId> = match inproc.get_session(session) {
        Ok(s) => s.panes.keys().copied().collect(),
        Err(_) => return,
    };
    let ts = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    for pane in panes {
        // Skip panes with no recording state (status=(false, 0)) —
        // exporting an empty buffer still produces a header-only
        // cast which isn't useful and just pollutes the dir.
        match inproc.pane_recording_status(pane) {
            Ok((_, events)) if events > 0 => {}
            _ => continue,
        }
        match inproc.export_pane_recording(pane) {
            Ok(cast) => {
                let path = std::path::Path::new(&expanded)
                    .join(format!("{session}-{ts}-{pane}.cast"));
                if let Err(e) = std::fs::write(&path, cast.as_bytes()) {
                    warn!(path = %path.display(), error = %e, "auto-flush: write failed");
                } else {
                    info!(path = %path.display(), pane = %pane, "auto-flushed recording");
                }
            }
            Err(_) => continue,
        }
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

    // ── #48c recording auto-flush on kill ──────────────────

    #[test]
    fn kill_session_with_recording_auto_dir_writes_cast() {
        let inproc = std::sync::Arc::new(InProcess::new());
        let tmp = tempfile_path();
        let mut cfg = tear_config::TearConfig::default();
        cfg.recording_auto_dir = Some(tmp.to_string_lossy().into());
        let live = std::sync::Arc::new(LiveConfig::default());
        live.replace(cfg);

        // Create a session + start recording on its first pane.
        let sid = inproc
            .new_session("auto-flush-test", "/bin/sh")
            .expect("new_session");
        let pane_id = *inproc
            .get_session(sid)
            .unwrap()
            .panes
            .keys()
            .next()
            .unwrap();
        inproc.enable_pane_recording(pane_id).unwrap();
        // Give the PTY a moment to produce a byte or two so the
        // recording has events (otherwise auto-flush skips empty
        // recordings by design).
        std::thread::sleep(std::time::Duration::from_millis(200));
        inproc
            .send_keys(pane_id, b"echo hello\n")
            .expect("send_keys");
        std::thread::sleep(std::time::Duration::from_millis(200));

        // Dispatch a KillSession via dispatch_with_config so the
        // auto-flush hook runs.
        match dispatch_with_config(&inproc, &live, Request::KillSession(sid), None) {
            Response::Ok => {}
            other => panic!("expected Ok, got {other:?}"),
        }

        // Walk the recording dir — must contain exactly one .cast
        // matching <sid>-*-<pane_id>.cast.
        let entries: Vec<_> = std::fs::read_dir(&tmp)
            .expect("read_dir")
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        let sid_prefix = format!("{sid}-");
        let hits: Vec<&String> = entries
            .iter()
            .filter(|n| n.starts_with(&sid_prefix) && n.ends_with(".cast"))
            .collect();
        assert_eq!(
            hits.len(),
            1,
            "expected one auto-flushed cast, found {entries:?}"
        );
        let _ = std::fs::remove_dir_all(&tmp);
    }

    fn tempfile_path() -> std::path::PathBuf {
        let mut p = std::env::temp_dir();
        let pid = std::process::id();
        let nonce: u64 = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos() as u64;
        p.push(format!("tear-auto-flush-{pid}-{nonce}"));
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    // ── #5 auth ──────────────────────────────────────────────────────

    #[test]
    fn ct_eq_returns_true_for_matching_bytes() {
        assert!(ct_eq(b"abc", b"abc"));
        assert!(ct_eq(b"", b""));
    }

    #[test]
    fn ct_eq_returns_false_for_differing_bytes_or_lengths() {
        assert!(!ct_eq(b"abc", b"abd"));
        assert!(!ct_eq(b"abc", b"abcd"));
        assert!(!ct_eq(b"abc", b""));
    }

    #[test]
    fn auth_required_rejects_other_requests_until_authenticate() {
        use crate::testing::{drain_responses, DuplexStream};
        use std::sync::mpsc::channel;

        // Pre-encode: (a) ListSessions before auth → Rejected.
        //             (b) Authenticate("wrong")     → Rejected.
        //             (c) Authenticate("secret")    → Ok.
        //             (d) ListSessions after auth   → Sessions([]).
        let mut input = Vec::new();
        write_msg(&mut input, &Request::ListSessions).unwrap();
        write_msg(&mut input, &Request::Authenticate("wrong".into())).unwrap();
        write_msg(&mut input, &Request::Authenticate("secret".into())).unwrap();
        write_msg(&mut input, &Request::ListSessions).unwrap();

        let (tx, rx) = channel::<u8>();
        let stream = DuplexStream::new(input, tx);
        let inproc = Arc::new(InProcess::new());
        let live = Arc::new(LiveConfig::default());
        let _ = serve_connection_with_auth(
            stream,
            inproc,
            live,
            None,
            Some("secret".into()),
        );

        let resps = drain_responses(&rx);
        assert_eq!(resps.len(), 4, "got: {resps:?}");
        assert!(matches!(resps[0], Response::Err(WireError::Rejected(_))));
        assert!(matches!(resps[1], Response::Err(WireError::Rejected(_))));
        assert!(matches!(resps[2], Response::Ok));
        assert!(matches!(resps[3], Response::Sessions(_)));
    }

    #[test]
    fn no_auth_required_accepts_requests_immediately() {
        use crate::testing::{drain_responses, DuplexStream};
        use std::sync::mpsc::channel;

        let mut input = Vec::new();
        write_msg(&mut input, &Request::ListSessions).unwrap();

        let (tx, rx) = channel::<u8>();
        let stream = DuplexStream::new(input, tx);
        let inproc = Arc::new(InProcess::new());
        let live = Arc::new(LiveConfig::default());
        let _ = serve_connection_with_auth(stream, inproc, live, None, None);

        let resps = drain_responses(&rx);
        assert!(matches!(resps.first(), Some(Response::Sessions(_))), "got: {resps:?}");
    }

    // ── extra coverage: re-authenticate, empty token, identify_idempotent ──

    #[test]
    fn re_authenticate_after_success_returns_ok_again() {
        use crate::testing::{drain_responses, DuplexStream};
        use std::sync::mpsc::channel;

        let mut input = Vec::new();
        write_msg(&mut input, &Request::Authenticate("k".into())).unwrap();
        write_msg(&mut input, &Request::Authenticate("k".into())).unwrap();

        let (tx, rx) = channel::<u8>();
        let stream = DuplexStream::new(input, tx);
        let inproc = Arc::new(InProcess::new());
        let live = Arc::new(LiveConfig::default());
        let _ = serve_connection_with_auth(stream, inproc, live, None, Some("k".into()));

        let resps = drain_responses(&rx);
        assert_eq!(resps.len(), 2);
        assert!(matches!(resps[0], Response::Ok));
        assert!(matches!(resps[1], Response::Ok));
    }

    #[test]
    fn pre_emptive_authenticate_on_no_auth_daemon_is_ok() {
        use crate::testing::{drain_responses, DuplexStream};
        use std::sync::mpsc::channel;

        let mut input = Vec::new();
        write_msg(&mut input, &Request::Authenticate("anything".into())).unwrap();

        let (tx, rx) = channel::<u8>();
        let stream = DuplexStream::new(input, tx);
        let inproc = Arc::new(InProcess::new());
        let live = Arc::new(LiveConfig::default());
        let _ = serve_connection_with_auth(stream, inproc, live, None, None);

        let resps = drain_responses(&rx);
        assert!(matches!(resps.first(), Some(Response::Ok)), "got: {resps:?}");
    }

    #[test]
    fn identify_client_is_idempotent_and_returns_ok() {
        use crate::testing::{drain_responses, DuplexStream};
        use std::sync::mpsc::channel;

        let mut input = Vec::new();
        write_msg(&mut input, &Request::IdentifyClient(1)).unwrap();
        write_msg(&mut input, &Request::IdentifyClient(99)).unwrap();
        write_msg(&mut input, &Request::ListSessions).unwrap();

        let (tx, rx) = channel::<u8>();
        let stream = DuplexStream::new(input, tx);
        let inproc = Arc::new(InProcess::new());
        let live = Arc::new(LiveConfig::default());
        let _ = serve_connection_with_auth(stream, inproc, live, None, None);

        let resps = drain_responses(&rx);
        assert_eq!(resps.len(), 3);
        assert!(matches!(resps[0], Response::Ok));
        assert!(matches!(resps[1], Response::Ok));
        assert!(matches!(resps[2], Response::Sessions(_)));
    }

    #[test]
    fn resolve_required_token_missing_env_errors() {
        let live = LiveConfig::default();
        let mut cfg = tear_config::TearConfig::default();
        // Pick an env var that is overwhelmingly unlikely to be set.
        cfg.auth_token_env = Some("__TEAR_NO_SUCH_VAR_FOR_TESTS__".into());
        live.replace(cfg);
        let err = resolve_required_token(&live).expect_err("missing env must error");
        assert_eq!(err.kind(), io::ErrorKind::PermissionDenied);
        let msg = format!("{err}");
        assert!(msg.contains("__TEAR_NO_SUCH_VAR_FOR_TESTS__"), "msg: {msg}");
    }

    #[test]
    fn resolve_required_token_none_when_unset() {
        let live = LiveConfig::default();
        assert!(resolve_required_token(&live).unwrap().is_none());
    }

    // ── audit log emit-through-dispatch coverage ─────────────────────

    #[test]
    fn dispatch_with_config_set_config_emits_set_config_audit_event() {
        let tmp = tempfile_path();
        let log_path = tmp.join("audit.jsonl");
        let audit = AuditLog::open(log_path.to_str().unwrap()).unwrap();
        let inproc = InProcess::new();
        let live = LiveConfig::default();

        // SetConfig YAML must parse as a valid TearConfig — use
        // the default round-tripped through YAML.
        let yaml = serde_yaml_ng::to_string(&tear_config::TearConfig::default()).unwrap();
        let resp = dispatch_with_config(
            &inproc,
            &live,
            Request::SetConfig(yaml),
            Some(&audit),
        );
        assert!(matches!(resp, Response::Ok));

        // Drop the audit handle so the file's BufWriter flushes.
        drop(audit);

        let content = std::fs::read_to_string(&log_path).unwrap();
        assert!(content.contains("set_config"), "audit: {content}");
        assert!(content.contains("config_hash"), "audit: {content}");

        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn dispatch_with_config_kill_session_emits_session_kill_event() {
        let tmp = tempfile_path();
        let log_path = tmp.join("audit.jsonl");
        let audit = AuditLog::open(log_path.to_str().unwrap()).unwrap();
        let inproc = InProcess::new();
        let live = LiveConfig::default();

        // Create a session, then kill it via dispatch_with_config.
        let sid = inproc.new_session("audit-kill-test", "/bin/sh").unwrap();
        let resp = dispatch_with_config(
            &inproc,
            &live,
            Request::KillSession(sid),
            Some(&audit),
        );
        assert!(matches!(resp, Response::Ok));
        drop(audit);

        let content = std::fs::read_to_string(&log_path).unwrap();
        assert!(content.contains("session_kill"), "audit: {content}");
        assert!(content.contains(&sid.to_string()), "audit: {content}");

        let _ = std::fs::remove_dir_all(&tmp);
    }
}
