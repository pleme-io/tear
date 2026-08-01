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
pub mod kanshou_state;
pub mod praca_store;
#[cfg(any(test, feature = "testing"))]
pub mod testing;

use std::io;
use std::net::{SocketAddr, TcpListener};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;

use tear_config::LiveConfig;
use tear_core::InProcess;
use tear_types::wire::{read_frame, write_msg, Framed, Request, Response, WireError};
use tear_types::shutai::{Declared, Shutai};
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
    /// M1 "Remember" — the praça persistence store (project↔session
    /// bindings + frecency, atomically persisted on every lifecycle
    /// mutation). Held here so tests + in-process consumers can inspect
    /// the live bindings without round-tripping the RPC.
    praca: PracaStore,
    /// Kept alive for the lifetime of the daemon so the notify
    /// watcher's spawned thread keeps running. Drop = stop
    /// watching.
    _config_watcher: Option<notify::RecommendedWatcher>,
}

/// Decide a new session's [`SessionSource`] from what the connection IS,
/// not only from what the request SAYS.
///
/// ## The bypass this closes
///
/// `SessionSource` drives `tear list --source agent` — the field an
/// operator uses to triage what automation started — and it also selects
/// the durability grace a detached session gets. Until now it was purely
/// caller-declared: a client passed `SessionSource::Human` and the daemon
/// recorded it. The thing being triaged chose its own label.
///
/// ## Precedence, and why it is this way round
///
/// An automation's own declaration WINS over a bare absence, because a
/// `Named("pleme-ci-deploy")` is strictly more informative than `Agent`,
/// and the automation is the only thing that knows its own name. What a
/// caller can no longer do is claim to be *less* automated than its
/// connection says: a connection that identified itself as an agent
/// cannot register a session as `Human`.
///
/// So the rule is: **a declaration may refine, never downgrade.**
///
/// ## Tier
///
/// **only-mitigated**, and the ceiling is the same one shutai carries: the
/// automation half of a shutai is itself a peer's claim (`Declared`), so a
/// same-uid process that never calls `IdentifyClient` still registers as
/// `Human`. This closes the *inconsistent* case — declaring one thing on
/// the connection and another in the request — not the *silent* one.
/// Closing that needs the peer credential to distinguish processes, which
/// it cannot: same-uid processes are mutually trusting by construction.
fn derive_session_source(
    declared: Option<tear_types::SessionSource>,
    shutai: Option<&Shutai>,
) -> tear_types::SessionSource {
    use tear_types::SessionSource;
    let from_conn = shutai.map(Shutai::session_source);
    match (declared, from_conn) {
        // The connection says automation; the request claims Human. The
        // connection wins — this is the downgrade the function exists to
        // refuse.
        (Some(SessionSource::Human) | None, Some(conn)) if conn != SessionSource::Human => conn,
        // Anything else the caller said stands: a Named label refines an
        // Agent, and a human connection has nothing to add.
        (Some(d), _) => d,
        (None, Some(conn)) => conn,
        (None, None) => SessionSource::default(),
    }
}

/// Mint a [`Shutai`] for a local connection from the peer credential the
/// KERNEL reports — never from anything the peer sent.
///
/// This is shutai's attested arm, and the whole reason identity roots in a
/// syscall rather than a credential plane: it costs one `getsockopt`, needs
/// no network, no PKI and no broker, and it is not forgeable by a payload.
///
/// Platform split, because the two kernels expose it differently:
/// Apple/BSD answer `LOCAL_PEERCRED` with an `XuCred`; Linux answers
/// `SO_PEERCRED` with a `UnixCredentials` that also carries a pid. Only the
/// uid is taken — a pid is a racy identifier (it can be recycled between
/// the read and any use of it) and nothing here needs one.
///
/// On a platform with neither, or a failed read, this returns
/// [`Shutai::remote`]: **no attestation is honest, a fabricated one is
/// not.** Degrading to "I could not tell" keeps the type's promise;
/// substituting the daemon's own uid would forge exactly the fact the type
/// exists to carry.
fn shutai_for_local_peer(stream: &UnixStream) -> Shutai {
    #[cfg(any(target_os = "macos", target_os = "ios", target_os = "freebsd"))]
    {
        match nix::sys::socket::getsockopt(stream, nix::sys::socket::sockopt::LocalPeerCred) {
            Ok(cred) => Shutai::from_peer_uid(cred.uid()),
            Err(e) => {
                warn!(error = %e, "peer credential unavailable; shutai is unattested");
                Shutai::remote()
            }
        }
    }
    #[cfg(target_os = "linux")]
    {
        match nix::sys::socket::getsockopt(stream, nix::sys::socket::sockopt::PeerCredentials) {
            Ok(cred) => Shutai::from_peer_uid(cred.uid()),
            Err(e) => {
                warn!(error = %e, "peer credential unavailable; shutai is unattested");
                Shutai::remote()
            }
        }
    }
    #[cfg(not(any(
        target_os = "macos",
        target_os = "ios",
        target_os = "freebsd",
        target_os = "linux"
    )))]
    {
        let _ = stream;
        Shutai::remote()
    }
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

    /// Borrow the M1 praça store — the project↔session bindings +
    /// frecency the daemon persists. Useful for tests that assert a
    /// binding was written, and for in-process consumers (mado) that
    /// want to query the persisted state for an auto-attach decision.
    pub fn praca(&self) -> &PracaStore {
        &self.praca
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
    // ── TCP refuses to bind open ────────────────────────────────
    //
    // A UDS is closed by its file mode; a TCP listener has no such
    // property, so the same `authed = required_token.is_none()`
    // default means an unauthenticated peer can spawn arbitrary
    // processes with arbitrary argv over the network.
    //
    // The doc comment above used to carry the whole mitigation —
    // "tunnel through SSH or run behind a TLS proxy". That is a
    // convention where a type belongs: nothing stopped
    // `tear daemon --tcp 0.0.0.0:7000` from doing exactly the thing
    // the sentence warns against, and the operator who most needs
    // the warning is the one who did not read it.
    //
    // So it is a REFUSAL now. Binding a non-loopback address with
    // no token configured returns an error instead of a listener.
    // Loopback stays permitted: it is reachable only by local uids,
    // which is the same trust boundary the UDS has.
    let requires_token = !addr.ip().is_loopback();
    if requires_token && live_config.load().auth_token_env.is_none() {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!(
                "refusing to bind {addr}: a non-loopback tear-daemon needs \
                 `auth_token_env` set, or every peer that can reach the port \
                 can spawn processes on this host. Bind a loopback address \
                 and tunnel it (ssh -L), or configure a token."
            ),
        ));
    }

    let listener = TcpListener::bind(addr)?;
    let bound = listener.local_addr()?;
    info!(
        addr = %bound,
        token_required = requires_token,
        "tear-daemon listening (tcp)"
    );

    let watcher = match live_config.spawn_watcher() {
        Ok(w) => Some(w),
        Err(e) => {
            warn!(error = %e, "config file watcher could not start (tcp daemon)");
            None
        }
    };

    let audit = open_audit_from_config(&live_config);
    let required_token = resolve_required_token(&live_config)?;
    // M1 "Remember": load the persisted praça bindings + frecency from
    // the default state path; mutations re-persist atomically.
    let praca = praca_store::PracaStore::open_default();

    let stop = Arc::new(AtomicBool::new(false));
    let stop_for_accept = stop.clone();
    let inproc_for_accept = inproc.clone();
    let config_for_accept = live_config.clone();
    let audit_for_accept = audit.clone();
    let token_for_accept = required_token.clone();
    let praca_for_accept = praca.clone();

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
                Some(praca_for_accept),
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
        praca,
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

    // ── Socket sovereignty ──────────────────────────────────────
    //
    // Connecting to a Unix socket requires the WRITE bit, so 0600
    // means only this uid can reach the daemon — which matters
    // because on the default configuration every connection is
    // authorized from frame 1 (`authed = required_token.is_none()`
    // below) and `NewSession`/`SplitPane` spawn arbitrary processes
    // with arbitrary argv. Reachability IS the access control.
    //
    // This was previously left to the ambient umask. A umask of 022
    // happens to yield 0755, which denies the write bit and is
    // therefore closed — but by accident of the environment rather
    // than by anything the code asked for. A umask of 0 yields 0777
    // and the same daemon is open to every local user. An invariant
    // that holds because of a shell setting is not an invariant.
    //
    // Note this makes the same-uid property structural WITHOUT a
    // peer-credential check: the kernel enforces it at connect(2).
    // `getpeereid` earns its keep for ATTRIBUTION (which principal
    // is acting — see theory/TEGATA.md's shutai), not for access.
    std::fs::set_permissions(&socket_path, std::fs::Permissions::from_mode(0o600))?;

    info!(path = %socket_path.display(), mode = "0600", "tear-daemon listening");

    // Stamp the bound socket path on the InProcess backend so
    // every PTY child it spawns can inherit `TEAR_SOCKET=<path>`
    // (alongside TEAR_SESSION_ID/NAME and TEAR_PANE_ID) — shells
    // and starship rely on it for prompt visibility + re-discovery.
    inproc.set_socket_path(socket_path.clone());

    // ── Kanshou introspection server ─────────────────────────────
    // Expose the daemon's live Registry (sessions, panes, socket,
    // process metadata) over a kanshou Unix socket so operator
    // tools and MCP servers query the actual state. See
    // pleme-io/kanshou. Spawned on a dedicated thread with its
    // own tokio runtime so the existing std::thread accept loop
    // here is untouched. Best-effort: bind failure logs and
    // continues — the daemon still serves its RPC.
    {
        let kanshou_state = Arc::new(kanshou_state::TearDaemonState::new(inproc.clone()));
        std::thread::Builder::new()
            .name("tear-kanshou".into())
            .spawn(move || {
                match tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .thread_name("tear-kanshou-tokio")
                    .build()
                {
                    Ok(rt) => rt.block_on(async {
                        match kanshou_state::spawn_server("tear-daemon", kanshou_state) {
                            Ok(path) => {
                                info!(
                                    socket = %path.display(),
                                    "kanshou introspection live"
                                );
                                std::future::pending::<()>().await;
                            }
                            Err(e) => {
                                warn!(error = %e, "kanshou bind failed; introspection disabled");
                            }
                        }
                    }),
                    Err(e) => {
                        warn!(error = %e, "could not create kanshou tokio runtime");
                    }
                }
            })
            .expect("spawn tear-kanshou thread");
    }

    // ── NO orphan pruner. Deliberately. ──────────────────────────
    // There used to be a `tear-orphan-prune` thread here: it ticked
    // every 2 s and killed any session whose panes had zero live
    // byte-stream subscribers, after a grace keyed on provenance —
    // 3 s for `SessionSource::Named`, 15 s for `Agent`, never for
    // `Human`. It was deleted 2026-07-31 because it destroyed live
    // sessions, which is the one thing a multiplexer must not do.
    //
    // Measured on a scratch daemon (three sessions, all shells
    // RUNNING, nobody attached):
    //
    //   t=+0s   S-human  S-named  S-agent
    //   t=+4s   S-human           S-agent   ← named killed, 3 s grace
    //   t=+16s  S-human                     ← agent killed, 15 s grace
    //
    // No RPC ever failed — the whole call path returned `Ok` and the
    // session was simply gone a moment later. banken's bancada
    // (`Named`) died ~3 s after opening, before an operator could
    // attach; mado's `tear_new_session` MCP tool (`Agent`) died at
    // ~15 s. "Detached" is the normal steady state of a multiplexer
    // session, not evidence of an orphan.
    //
    // The stated motive — a crashed consumer leaving a live PTY +
    // reader thread + scrollback behind — does not justify it either:
    // when mado crashes its SHELLS ARE STILL ALIVE, and surviving a
    // dead front-end so the operator can re-attach is the feature,
    // not the leak. The genuine leak class (a session whose every
    // child has EXITED and that nobody was watching) is already
    // handled at its cause, on the pane's own exit path, by
    // `tear-core`'s reap — keyed on child-process death, never on
    // attachment. `InProcess::reap_proven_dead_session` +
    // `tear_core::AllPanesExited` are the only door back in, and that
    // ticket cannot be minted from a subscriber count.
    //
    // Sealed by `tear/tests/session_durability.rs` (it lives in the
    // CLI crate because that is where the daemon harness and a real
    // `tear-client` already meet).

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
    // M1 "Remember": load the persisted praça bindings + frecency from
    // the default state path; mutations re-persist atomically.
    let praca = praca_store::PracaStore::open_default();

    let stop = Arc::new(AtomicBool::new(false));
    let stop_for_accept = stop.clone();
    let inproc_for_accept = inproc.clone();
    let socket_for_accept = socket_path.clone();
    let config_for_accept = live_config.clone();
    let audit_for_accept = audit.clone();
    let token_for_accept = required_token.clone();
    let praca_for_accept = praca.clone();

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
                Some(praca_for_accept),
            )
        })?;

    Ok(DaemonHandle {
        socket_path,
        stop,
        accept_thread: Some(accept_thread),
        _inproc: inproc,
        config: live_config,
        audit,
        praca,
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
    praca: Option<PracaStore>,
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
                let praca_for_conn = praca.clone();
                let _ = thread::Builder::new()
                    .name("tear-daemon-conn-tcp".into())
                    .spawn(move || {
                        // Per-connection: switch back to blocking
                        // mode (set_nonblocking on TcpListener is
                        // inherited by accepted TcpStream).
                        let _ = stream.set_nonblocking(false);
                        if let Err(e) = serve_connection_full(
                            stream,
                            inproc_for_conn,
                            config_for_conn,
                            audit_for_conn,
                            token_for_conn,
                            praca_for_conn,
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
    praca: Option<PracaStore>,
) {
    for incoming in listener.incoming() {
        if stop.load(Ordering::SeqCst) {
            debug!("accept loop: stop requested");
            return;
        }
        match incoming {
            Ok(stream) => {
                // shutai's attested arm, read BEFORE the connection is
                // served: whatever the peer sends afterwards cannot
                // influence what the kernel already told us. Derived here
                // rather than inside the serve loop so there is exactly one
                // site where a local identity is minted.
                let shutai = shutai_for_local_peer(&stream);
                debug!(attested = ?shutai.attested(), "connection accepted");
                let inproc_for_conn = inproc.clone();
                let config_for_conn = config.clone();
                let audit_for_conn = audit.clone();
                let token_for_conn = required_token.clone();
                let praca_for_conn = praca.clone();
                let _ = thread::Builder::new()
                    .name("tear-daemon-conn".into())
                    .spawn(move || {
                        // The UDS path carries the kernel-attested uid.
                        // The TCP loop above deliberately does NOT — there
                        // is no peer credential on a socket the kernel did
                        // not broker locally, and `remote()` says so.
                        if let Err(e) = serve_connection_shutai(
                            stream,
                            inproc_for_conn,
                            config_for_conn,
                            audit_for_conn,
                            token_for_conn,
                            praca_for_conn,
                            shutai,
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
    stream: S,
    inproc: Arc<InProcess>,
    config: Arc<LiveConfig>,
    audit: Option<AuditLog>,
    required_token: Option<String>,
) -> io::Result<()> {
    serve_connection_full(stream, inproc, config, audit, required_token, None)
}

/// Full serve loop — like [`serve_connection_with_auth`] but also
/// carries the M1 praça [`PracaStore`] so session create/kill mutate +
/// persist the project↔session bindings + frecency. The daemon's own
/// accept loops call this; the back-compat [`serve_connection`] /
/// [`serve_connection_with_auth`] entry points pass `praca = None` so
/// external embedders (tear-client, the test harness) keep their
/// signatures.
pub fn serve_connection_full<S: io::Read + io::Write>(
    stream: S,
    inproc: Arc<InProcess>,
    config: Arc<LiveConfig>,
    audit: Option<AuditLog>,
    required_token: Option<String>,
    praca: Option<PracaStore>,
) -> io::Result<()> {
    // No peer credential available on a bare `Read + Write` — an embedder
    // hand-stitching a transport has no socket for the kernel to attest.
    // `remote()` is the honest answer; see `shutai_for_local_peer`.
    serve_connection_shutai(
        stream,
        inproc,
        config,
        audit,
        required_token,
        praca,
        Shutai::remote(),
    )
}

/// Serve a connection whose acting entity is already known.
///
/// A separate entry point rather than a changed signature, per ★★
/// MODULARIZE, DON'T DELETE: every existing caller — including external
/// embedders and `tear-client`'s test harness — keeps compiling, and the
/// shutai-aware path is opted into rather than forced.
///
/// The daemon's own accept loop calls THIS one, so a real UDS connection
/// carries a kernel-attested uid while a hand-stitched stream honestly
/// reports `remote()`.
pub fn serve_connection_shutai<S: io::Read + io::Write>(
    mut stream: S,
    inproc: Arc<InProcess>,
    config: Arc<LiveConfig>,
    audit: Option<AuditLog>,
    required_token: Option<String>,
    praca: Option<PracaStore>,
    shutai: Shutai,
) -> io::Result<()> {
    let mut authed = required_token.is_none();
    // #2 — per-connection client identity. Set by IdentifyClient;
    // read on SendKeys to enforce Leader policy.
    let mut client_id: Option<u64> = None;
    // shutai for THIS connection. The attested half arrived from the
    // kernel and is never rewritten; only the declared half can change,
    // and only via a peer's own claim.
    let mut shutai = shutai;
    loop {
        // A payload we cannot decode is NOT a stream we cannot read.
        // `read_frame` keeps them apart: an unknown variant (the
        // shape a newer client's request takes) consumed its whole
        // frame and left the stream aligned, so it gets a legible
        // refusal and the connection stays up. Only a genuine
        // transport failure ends the loop.
        //
        // This used to be `Err(e) => return Err(e)` over `read_msg`,
        // which collapsed both cases — so the FIRST client to send a
        // variant this daemon had never heard of got a bare
        // connection close with no diagnostic. That made adding any
        // Request variant a flag day. It no longer is, from this
        // build forward; daemons older than this still hang up,
        // which is why `tear-client`'s probe tolerates the loss.
        let req: Request = match read_frame(&mut stream) {
            Ok(Framed::Msg(r)) => r,
            Ok(Framed::Undecodable { reason, len }) => {
                warn!(
                    %reason,
                    frame_len = len,
                    authed,
                    "undecodable request frame — refusing it and keeping the connection"
                );
                // The frame is refused either way; only the *detail*
                // is gated. This arm runs BEFORE the auth check below
                // (it has to — we could not decode enough to know what
                // was asked), and serde's reason enumerates every
                // variant this build knows while the version names the
                // build itself. Neither goes to an unauthenticated
                // peer. The daemon's log keeps the full diagnostic
                // regardless, which is where an operator looks.
                let detail = if authed {
                    format!(
                        "this daemon (tear-daemon {}) cannot decode that request: {reason}",
                        env!("CARGO_PKG_VERSION")
                    )
                } else {
                    "unable to decode request; authenticate first".to_owned()
                };
                write_msg(&mut stream, &Response::Err(WireError::Rejected(detail)))?;
                continue;
            }
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
            // A connection that identifies itself is, by every existing
            // caller's convention, an automation — the Leader policy
            // exists precisely so one identified client drives a pane
            // while a human watches. Record that as a DECLARATION.
            //
            // Note what this does NOT do: it cannot touch the attested
            // half, so a peer naming itself an agent never rewrites what
            // the kernel said about its uid. The claim is recorded at its
            // own tier and nowhere else.
            shutai = shutai.clone().declaring(Declared::Agent {
                label: Some(id.to_string()),
            });
            write_msg(&mut stream, &Response::Ok)?;
            continue;
        }
        // #2 — Leader-policy gate. Before forwarding SendKeys to the
        // InProcess (whose own `Locked` check still runs after), peek
        // at the pane's input_policy. If it's `Leader(want)` and the
        // connection's client_id doesn't match, reject locally.
        if let Request::SendKeys { id, .. } = &req {
            // Resolved through `TearSession::admits`, NOT by reading
            // `pane.input_policy` here. That distinction is the whole
            // point: the brake lives on the session, so a gate that peeks
            // at the pane alone cannot see it — an agent-driven pane would
            // sail straight through a freio the operator had engaged.
            let admission = inproc
                .with_registry(|r| r.locate_pane(*id).and_then(|(sid, _)| {
                    r.sessions.get(&sid).and_then(|s| s.admits(*id))
                }));
            match admission {
                Some(tear_types::Admission::Refuse(tear_types::RefusalReason::Freio)) => {
                    let resp = Response::Err(tear_types::wire::WireError::Rejected(format!(
                        "freio engaged: pane {id:?} is agent-driven and braked. \
                         The operator releases it with `tear freio --release`."
                    )));
                    write_msg(&mut stream, &resp)?;
                    continue;
                }
                Some(tear_types::Admission::OnlyLeader { id: want }) if client_id != Some(want) => {
                    let resp = Response::Err(tear_types::wire::WireError::Rejected(format!(
                        "leader policy: pane {id:?} requires client_id={want}, \
                         connection identified as {client_id:?}"
                    )));
                    write_msg(&mut stream, &resp)?;
                    continue;
                }
                // `Refuse(Policy)` deliberately falls through: tear-core's
                // own `Locked` check still runs and owns that refusal. Two
                // gates answering one question is the shape being retired,
                // not widened — this one takes the cases tear-core
                // structurally cannot see (client identity, and now the
                // session-level brake).
                _ => {}
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
        let resp = dispatch_with_shutai(
            &inproc,
            &config,
            req,
            audit.as_ref(),
            praca.as_ref(),
            Some(&shutai),
        );
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
    // ── engate M0: initial-grid replay ──────────────────────────────
    //
    // Race the daemon used to lose: shell prints prompt at t=0;
    // mado attaches at t+10ms; mado's subscribe channel only sees
    // bytes emitted AFTER attach; mado's local terminal model stays
    // empty even though tear's grid is full. Fix: snapshot the
    // pane's current grid right after subscribe, serialize as ANSI
    // bytes (`PaneSnapshot::to_ansi()`), and emit as a single
    // `PaneBytes` frame BEFORE entering the live stream loop.
    // Consumers feed it through their VT parser unchanged — their
    // model converges to the current grid before the first redraw.
    //
    // The snapshot+subscribe ordering matters: subscribe FIRST so
    // no bytes that arrive between snapshot and subscribe-register
    // are lost. The snapshot may include a few bytes of overlap
    // with the live stream — harmless, since ANSI sequences are
    // idempotent at the parser level (re-rendering the same cells
    // is a no-op).
    //
    // Long-term: engate M1 lifts this into a typed
    // `SubscribeResponse { initial_grid, then_stream }` shape,
    // statig-enforced typestate, loom-tested across all
    // (subscribe, emit) interleavings.
    match inproc.pane_snapshot(pane) {
        Ok(snap) => {
            let bytes = snap.to_ansi();
            if !bytes.is_empty()
                && write_msg(&mut stream, &Response::PaneBytes(bytes)).is_err()
            {
                return Ok(());
            }
        }
        Err(e) => {
            // Pane vanished between subscribe and snapshot — rare,
            // but possible if the shell exited in that window. Let
            // the live-stream loop handle the closure path; the
            // consumer just won't get an initial replay.
            tracing::debug!(?e, %pane, "pane_snapshot failed at subscribe time — skipping initial replay");
        }
    }
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
        Request::NewSession { name, shell, source, size_cells, args } => {
            let src = source.unwrap_or_default();
            let size = size_cells.unwrap_or((80, 24));
            map_result(
                inproc.new_session_with_source_and_size(&name, &shell, &args, src, size),
                Response::SessionId,
            )
        }
        Request::RenameSession { id, new_name } => {
            map_unit(inproc.rename_session(id, &new_name))
        }
        Request::KillSession(id) => map_unit(inproc.kill_session(id)),
        Request::NewWindow { session, name, shell, args } => {
            map_result(
                inproc.new_window(session, &name, &shell, &args),
                Response::WindowId,
            )
        }
        Request::KillWindow(id) => map_unit(inproc.kill_window(id)),
        Request::SelectWindow(id) => map_unit(inproc.select_window(id)),
        Request::SplitPane { origin, direction, shell, args } => {
            map_result(
                inproc.split_pane(origin, direction, &shell, &args),
                Response::PaneId,
            )
        }
        Request::KillPane(id) => map_unit(inproc.kill_pane(id)),
        Request::SelectPane(id) => map_unit(inproc.select_pane(id)),
        Request::ResizePane { id, direction, delta_cells } => {
            map_unit(inproc.resize_pane(id, direction, delta_cells))
        }
        Request::ApplyLayout { window, kind } => map_unit(inproc.apply_layout(window, kind)),
        Request::SendKeys { id, bytes } => map_unit(inproc.send_keys(id, &bytes)),
        Request::SetFreio { session, engaged } => {
            let (sessions, braked, unbrakable) = inproc.set_freio(session, engaged);
            Response::Freio { sessions, braked, unbrakable }
        }
        Request::GetFreio => Response::Freio {
            sessions: inproc.freio_state(),
            braked: Vec::new(),
            unbrakable: Vec::new(),
        },
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
        // Push the embedder's typed env + cwd override onto the daemon's
        // InProcess so every SUBSEQUENT NewSession spawn's child PTY sees
        // the embedder's capability set (TERM/COLORTERM/TERMINFO/
        // TERM_PROGRAM + stamped PWD) AFTER the inherited + fallback env.
        // Mirrors how the embedded path already calls set_spawn_env; this
        // is the daemon-transport equivalent. No LiveConfig needed, so it
        // lives here in `dispatch` (dispatch_with_config falls through to
        // it). Applied via interior mutability — set_spawn_env takes &self.
        Request::SetSpawnEnv(env) => {
            inproc.set_spawn_env(env);
            Response::Ok
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
        // The capability probe answers identically here and in
        // `dispatch_with_config` — a consumer driving the bare
        // dispatcher must not see a different daemon identity than
        // one going through the socket.
        Request::Hello { .. } => Response::Hello(tear_types::DaemonHello::for_this_build(env!(
            "CARGO_PKG_VERSION"
        ))),
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
    praca: Option<&PracaStore>,
) -> Response {
    // No connection to derive an actor from — an embedder calling the
    // simple entry point gets the pre-shutai behaviour.
    dispatch_with_shutai(inproc, config, req, audit, praca, None)
}

/// Dispatch knowing who is asking.
///
/// A separate entry point rather than a changed signature, per ★★
/// MODULARIZE, DON'T DELETE — five existing callers keep compiling.
///
/// The only thing `shutai` currently changes is [`SessionSource`]
/// derivation, and that is the point of it: see the `NewSession` arm.
pub fn dispatch_with_shutai(
    inproc: &InProcess,
    config: &LiveConfig,
    req: Request,
    audit: Option<&AuditLog>,
    praca: Option<&PracaStore>,
    shutai: Option<&Shutai>,
) -> Response {
    match req {
        // The capability probe. Answers with this DAEMON's own
        // version — not the caller's — plus every capability this
        // build implements. Handled in dispatch rather than in the
        // serve loop so the embedded/in-process path and the test
        // harness answer it identically.
        Request::Hello { client_version } => {
            debug!(
                %client_version,
                daemon_version = env!("CARGO_PKG_VERSION"),
                "capability probe"
            );
            Response::Hello(tear_types::DaemonHello::for_this_build(env!(
                "CARGO_PKG_VERSION"
            )))
        }
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
                // M1 "Remember": drop the killed session's record +
                // bindings so no project resolves to a dead session.
                if let Some(store) = praca {
                    praca_on_session_kill(store, id);
                }
            }
            resp
        }
        Request::NewSession { name, shell, source, size_cells, args } => {
            let src = derive_session_source(source, shutai);
            let size = size_cells.unwrap_or((80, 24));
            let result =
                inproc.new_session_with_source_and_size(&name, &shell, &args, src.clone(), size);
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
                // M1 "Remember": bind project_root → session + bump
                // frecency. The project root is the walk-up of the
                // embedder's spawn cwd (the dir mado opened it in).
                if let Some(store) = praca {
                    praca_on_session_create(store, inproc, *sid, now_unix());
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
        // Rename also updates the praça record's custom name so the Ctrl-S
        // picker + search reflect it (the operator "jumped in and named it").
        Request::RenameSession { id, new_name } => {
            let resp = map_unit(inproc.rename_session(id, &new_name));
            if matches!(resp, Response::Ok) {
                if let Some(store) = praca {
                    praca_on_session_rename(store, id, &new_name);
                }
            }
            resp
        }
        other => dispatch(inproc, other),
    }
}

/// Apply an operator rename to the praça record's `custom_name` and persist
/// it — so the Ctrl-S picker shows the chosen name and search matches it at
/// the highest tier. A no-op if the session has no record yet.
fn praca_on_session_rename(store: &PracaStore, id: tear_types::SessionId, new_name: &str) {
    store.mutate(|p| {
        if let Some(rec) = p.index.get_mut(id) {
            rec.rename(new_name);
        }
    });
    debug!(session = %id, "praca: renamed session record");
}

// ── M1 "Remember" — praça lifecycle hooks ────────────────────────────
//
// These two helpers are the daemon-side wiring of the `praca` substrate:
// they translate a session-lifecycle mutation the daemon just performed
// into a `praca::Praca` update + an atomic persist, so project↔session
// bindings + frecency survive a daemon restart. Both are no-ops when no
// `PracaStore` is configured (the `Option<&PracaStore>` mirrors how the
// audit log threads through `dispatch_with_config`). Time is injected by
// the caller via [`now_unix`] — the substrate never reads the clock.

use crate::praca_store::PracaStore;

/// Record a freshly-created session in the praça store: derive the
/// project root from the daemon's current spawn cwd (the embedder's
/// `SpawnEnv.cwd` — the directory mado opened the session in), build a
/// [`praca::SessionRecord`] with the deterministic auto-name seed, bump
/// its visit/frecency, and bind `project_root → id`. Persisted atomically.
///
/// When no spawn cwd is set (the bare-daemon path — no embedder cwd
/// override), there is no project to bind, so the hook is skipped: the
/// binding memory is meaningless without a real directory.
fn praca_on_session_create(
    store: &PracaStore,
    inproc: &InProcess,
    id: tear_types::SessionId,
    now: u64,
) {
    let Some(cwd) = inproc.spawn_cwd() else {
        debug!(session = %id, "praca: no spawn cwd — skipping binding");
        return;
    };
    // A recognized project → a path-STABLE name (cd here always re-attaches
    // the same `🌊 tide`). A scratch / non-project cwd → a RANDOM THEMED
    // emoji name (frost or brazil, chosen by the session's own id) — the
    // operator's "create a session, by default a random themed emoji".
    let project = praca::find_project_root(&cwd);
    store.mutate(|p| {
        // Preserve frecency across a re-create under the same id: a raw
        // constructor resets visits to 1, which would lose the accumulated
        // count if this id was seen before. Carry prior visits forward.
        let prior_visits = p.index.get(id).map_or(0, |r| r.visits);
        let (mut rec, anchor) = match &project {
            Some(root) => (
                praca::SessionRecord::for_project(id, root.clone(), p.name_style, now),
                root.clone(),
            ),
            None => {
                let seed = id.0; // the session's unique u64 — varies per session
                let theme = if seed % 2 == 0 {
                    praca::SessionTheme::Frost
                } else {
                    praca::SessionTheme::Brazil
                };
                (
                    praca::SessionRecord::for_adhoc(id, seed, theme, cwd.clone(), p.name_style, now),
                    cwd.clone(),
                )
            }
        };
        // The constructors initialise visits = 1 (this open). Fold in any
        // prior count so frecency reflects total opens of this session.
        rec.visits = rec.visits.saturating_add(prior_visits);
        rec.cwd.clone_from(&cwd);
        p.index.upsert(rec);
        // Bind the anchor (project root, or the cwd for ad-hoc) so a later
        // cd back to it re-attaches this session.
        p.binding.bind(anchor, id);
    });
    debug!(session = %id, ?project, "praca: registered session");
}

/// Drop a killed session from the praça store: remove its record from
/// the index and drop every binding that pointed at it, so no project
/// keeps resolving to a session that no longer exists. Persisted
/// atomically.
fn praca_on_session_kill(store: &PracaStore, id: tear_types::SessionId) {
    store.mutate(|p| {
        p.index.remove(id);
        p.binding.remove_session(id);
    });
    debug!(session = %id, "praca: unbound killed session");
}

/// Current unix-seconds — the daemon's single clock-read point for
/// praça frecency stamping. The pure substrate takes this as an
/// injected `u64`; only the daemon edge reads the real clock.
fn now_unix() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
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
    // The serve loop reads via `read_frame` now; the tests still
    // decode single frames the simple way.
    use tear_types::wire::read_msg;

    /// ── freio, end to end through the daemon ────────────────────
    ///
    /// The type-level rows live in `tear-types`. These prove the brake is
    /// REACHABLE: that a wire request reaches the registry and that the
    /// SendKeys gate consults the session rather than peeking at the pane.
    #[test]
    fn set_freio_engages_and_reports_what_it_could_not_reach() {
        let inproc = InProcess::new();
        let sid = inproc
            .new_session_yurai(
                "braked",
                "/bin/sh",
                &[],
                tear_types::SessionSource::Agent,
                (80, 24),
                tear_types::Yurai::Automation { label: Some("claude".into()) },
            )
            .unwrap();

        let resp = dispatch(&inproc, Request::SetFreio { session: None, engaged: true });
        let Response::Freio { sessions, braked, unbrakable } = resp else {
            panic!("expected Response::Freio, got {resp:?}");
        };
        assert!(
            sessions.iter().any(|(id, f)| *id == sid && f.is_engaged()),
            "the session must record the brake"
        );
        assert_eq!(braked.len(), 1, "the agent pane is braked");
        assert!(
            unbrakable.is_empty(),
            "every pane here has known provenance"
        );

        // And the gate agrees.
        let pane = braked[0];
        let admission = inproc.with_registry(|r| {
            r.locate_pane(pane)
                .and_then(|(s, _)| r.sessions.get(&s).and_then(|s| s.admits(pane)))
        });
        assert_eq!(
            admission,
            Some(tear_types::Admission::Refuse(tear_types::RefusalReason::Freio))
        );
    }

    /// ★ The honest miss, reported through the wire rather than hidden.
    #[test]
    fn a_pane_of_unknown_provenance_is_reported_as_unreached() {
        let inproc = InProcess::new();
        // The plain trait verb — no provenance, which is exactly the
        // tmux-backend / pre-yurai case.
        inproc.new_session("legacy", "/bin/sh").unwrap();

        let resp = dispatch(&inproc, Request::SetFreio { session: None, engaged: true });
        let Response::Freio { braked, unbrakable, .. } = resp else {
            panic!("expected Response::Freio");
        };
        assert!(braked.is_empty(), "nothing to brake — provenance unknown");
        assert_eq!(
            unbrakable.len(),
            1,
            "the operator MUST be told this pane kept accepting input;              silence here would let them believe everything stopped"
        );
    }

    /// Release clears the brake rather than setting panes free.
    #[test]
    fn releasing_the_brake_restores_input() {
        let inproc = InProcess::new();
        inproc
            .new_session_yurai(
                "a",
                "/bin/sh",
                &[],
                tear_types::SessionSource::Agent,
                (80, 24),
                tear_types::Yurai::Automation { label: None },
            )
            .unwrap();

        let _ = dispatch(&inproc, Request::SetFreio { session: None, engaged: true });
        let _ = dispatch(&inproc, Request::SetFreio { session: None, engaged: false });

        let resp = dispatch(&inproc, Request::GetFreio);
        let Response::Freio { sessions, .. } = resp else {
            panic!("expected Response::Freio");
        };
        assert!(
            sessions.iter().all(|(_, f)| !f.is_engaged()),
            "release must clear every brake"
        );
    }

    /// ── SessionSource derivation rows ───────────────────────────
    ///
    /// The field an operator uses to triage what automation started was
    /// set by the thing being triaged. These pin the precedence rule: a
    /// declaration may REFINE, never DOWNGRADE.
    #[test]
    fn an_agent_connection_cannot_register_a_session_as_human() {
        use tear_types::SessionSource;
        let agent = Shutai::from_peer_uid(501).declaring(Declared::Agent { label: None });

        // The bypass, attempted directly: claim Human on an agent conn.
        assert_eq!(
            derive_session_source(Some(SessionSource::Human), Some(&agent)),
            SessionSource::Agent,
            "an automation must not be able to label itself Human — this is \
             the bypass the derivation exists to close"
        );
        // And saying nothing at all yields the connection's own answer.
        assert_eq!(
            derive_session_source(None, Some(&agent)),
            SessionSource::Agent
        );
    }

    /// A declaration REFINES: an automation naming itself is strictly more
    /// informative than the generic `Agent`, and it is the only thing that
    /// knows its own name.
    #[test]
    fn a_named_automation_keeps_its_own_label() {
        use tear_types::SessionSource;
        let agent = Shutai::from_peer_uid(501).declaring(Declared::Agent { label: None });
        assert_eq!(
            derive_session_source(Some(SessionSource::Named("pleme-ci".into())), Some(&agent)),
            SessionSource::Named("pleme-ci".into()),
            "a specific label must survive — refinement is allowed"
        );
    }

    /// A human connection adds nothing, and the pre-shutai path is
    /// unchanged: no connection means the caller's word stands.
    #[test]
    fn a_human_connection_and_no_connection_both_preserve_the_request() {
        use tear_types::SessionSource;
        let human = Shutai::from_peer_uid(501).declaring(Declared::Human);
        assert_eq!(
            derive_session_source(Some(SessionSource::Human), Some(&human)),
            SessionSource::Human
        );
        assert_eq!(
            derive_session_source(Some(SessionSource::Agent), None),
            SessionSource::Agent,
            "with no connection to derive from, the request is all there is"
        );
        assert_eq!(
            derive_session_source(None, None),
            SessionSource::default(),
            "and the default is unchanged for pre-shutai callers"
        );
    }

    /// A declaration NEVER rewrites what the kernel attested.
    ///
    /// This is the property the whole two-half split exists to guarantee,
    /// asserted end-to-end rather than on the type in isolation: a peer
    /// sends `IdentifyClient`, the connection records the claim, and the
    /// attested uid is byte-identical before and after.
    ///
    /// If these were one flattened enum, "I am an agent" and "the kernel
    /// says uid 501" would occupy the same field and the second could be
    /// overwritten by the first. That is the bug this test would catch.
    #[test]
    fn a_declaration_cannot_overwrite_the_attested_half() {
        let attested = Shutai::from_peer_uid(501);
        let declared = attested.clone().declaring(Declared::Agent {
            label: Some("42".into()),
        });

        assert_eq!(
            declared.attested(),
            attested.attested(),
            "declaring changed the attested half — the halves are not \
             actually separated"
        );
        assert!(declared.is_automation());
        assert!(!attested.is_automation());
        // And the derived provenance follows the declaration, not the uid.
        assert_eq!(
            declared.session_source(),
            tear_types::SessionSource::Named("42".into())
        );
    }

    /// shutai's attested arm reads the REAL uid from the kernel.
    ///
    /// The daemon connects to its own socket, so the peer credential must
    /// come back as this process's uid. That is the whole property: an
    /// identity the payload cannot influence, for one syscall, with no
    /// network.
    ///
    /// A wrong-but-plausible implementation — returning the daemon's own
    /// `getuid()` instead of the peer's — would also pass this test, since
    /// here they are the same process. That is a real limit of a
    /// same-machine test and it is why `shutai_for_local_peer` degrades to
    /// `remote()` rather than substituting a fabricated uid: the honest
    /// failure is "I could not tell", never a forged answer.
    #[test]
    fn a_local_connection_is_attested_with_the_peers_real_uid() {
        let dir = std::env::temp_dir().join(format!(
            "tear-shutai-{}-{}",
            std::process::id(),
            line!()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let sock = dir.join("t.sock");

        let inproc = Arc::new(InProcess::new());
        let handle = start(sock.clone(), inproc).expect("daemon starts");

        let stream = UnixStream::connect(&sock).expect("connect to own daemon");
        let shutai = shutai_for_local_peer(&stream);

        match shutai.attested() {
            tear_types::shutai::Attested::LocalUid { uid } => {
                let me = std::process::id();
                assert!(
                    *uid > 0 || me > 0,
                    "a uid of 0 on a non-root test run means the read failed"
                );
            }
            other => panic!(
                "a Unix-socket peer must be attested, got {other:?} — the \
                 platform arm is missing or getsockopt failed"
            ),
        }
        assert_eq!(
            *shutai.declared(),
            tear_types::shutai::Declared::Unknown,
            "a fresh connection has declared nothing"
        );

        drop(stream);
        handle.stop();
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// ── Socket sovereignty rows ─────────────────────────────────
    ///
    /// On the default configuration `authed = required_token.is_none()`
    /// is TRUE from frame 1, and `NewSession`/`SplitPane` spawn arbitrary
    /// processes with arbitrary argv. There is nothing to gate — spawning
    /// processes is what a terminal daemon is for. So the control is
    /// REACHABILITY: who can open the socket at all.
    #[test]
    fn the_socket_is_0600_so_only_this_uid_can_reach_the_daemon() {
        let dir = std::env::temp_dir().join(format!(
            "tear-sock-perm-{}-{}",
            std::process::id(),
            line!()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let sock = dir.join("t.sock");

        let inproc = Arc::new(InProcess::new());
        let handle = start(sock.clone(), inproc).expect("daemon starts");

        let mode = std::fs::metadata(&sock).unwrap().permissions().mode() & 0o777;
        assert_eq!(
            mode, 0o600,
            "socket mode is {mode:o}, not 0600 — connect(2) needs the WRITE \
             bit, so anything granting it to group/other lets another local \
             uid spawn processes on this host"
        );

        handle.stop();
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The mode must come from the CODE, not from whatever umask the
    /// operator's shell happened to have. This is the row that would have
    /// gone red before the fix: a umask of 022 yields 0755, which denies
    /// the write bit and therefore *looks* fine while being an accident of
    /// the environment.
    #[test]
    fn the_mode_is_not_inherited_from_a_permissive_umask() {
        let dir = std::env::temp_dir().join(format!(
            "tear-sock-umask-{}-{}",
            std::process::id(),
            line!()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o777)).unwrap();
        let sock = dir.join("t.sock");

        let inproc = Arc::new(InProcess::new());
        let handle = start(sock.clone(), inproc).expect("daemon starts");

        let mode = std::fs::metadata(&sock).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode & 0o077, 0, "group/other bits present: {mode:o}");

        handle.stop();
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A UDS is closed by its file mode; a TCP listener has no such
    /// property. Binding one to a routable address with no token means an
    /// unauthenticated peer can spawn processes over the network — so it
    /// is refused rather than documented.
    #[test]
    fn tcp_refuses_a_non_loopback_bind_without_a_token() {
        let inproc = Arc::new(InProcess::new());
        let addr: SocketAddr = "0.0.0.0:0".parse().unwrap();
        // `DaemonHandle` is not Debug, so match rather than expect_err.
        let err = match start_tcp(addr, inproc) {
            Err(e) => e,
            Ok(_) => panic!(
                "binding a routable address with no auth_token_env must be refused"
            ),
        };
        assert_eq!(err.kind(), io::ErrorKind::PermissionDenied);
        assert!(
            err.to_string().contains("auth_token_env"),
            "the refusal must name the fix: {err}"
        );
    }

    /// Loopback stays permitted — it is reachable only by local uids,
    /// which is the same trust boundary the UDS has. Refusing it would
    /// break `ssh -L` tunnelling, which is the sanctioned path.
    #[test]
    fn tcp_still_binds_loopback_without_a_token() {
        let inproc = Arc::new(InProcess::new());
        let addr: SocketAddr = "127.0.0.1:0".parse().unwrap();
        let handle = start_tcp(addr, inproc).expect("loopback needs no token");
        handle.stop();
    }

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
                size_cells: None,
                args: Vec::new(),
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
                size_cells: None,
                args: Vec::new(),
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
        match dispatch_with_config(&inproc, &live, Request::KillSession(sid), None, None) {
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
            None,
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
            None,
        );
        assert!(matches!(resp, Response::Ok));
        drop(audit);

        let content = std::fs::read_to_string(&log_path).unwrap();
        assert!(content.contains("session_kill"), "audit: {content}");
        assert!(content.contains(&sid.to_string()), "audit: {content}");

        let _ = std::fs::remove_dir_all(&tmp);
    }

    // ── M1 "Remember" — praça lifecycle persistence ─────────────────

    /// A unique praça store path under a temp dir.
    fn praca_temp_path(tag: &str) -> std::path::PathBuf {
        let mut p = std::env::temp_dir();
        let pid = std::process::id();
        let nonce: u128 = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        p.push(format!("tear-praca-daemon-{tag}-{pid}-{nonce}"));
        std::fs::create_dir_all(&p).unwrap();
        p.join("praca.json")
    }

    #[test]
    fn new_session_via_dispatch_binds_project_root_to_persisted_store() {
        let path = praca_temp_path("new-session-binds");
        let store = crate::praca_store::PracaStore::open(path.clone());
        let inproc = InProcess::new();
        let live = LiveConfig::default();

        // Stamp the embedder's spawn cwd — the directory mado opened the
        // session in (a child of the project root, to exercise walk-up).
        // No marker on disk in temp_dir, so project_root resolves to the
        // cwd itself; bind it deterministically.
        let cwd = path.parent().unwrap().to_path_buf();
        inproc.set_spawn_env(
            tear_types::SpawnEnv::none().with_cwd(Some(cwd.to_string_lossy().into())),
        );
        // The hook binds the WALK-UP root of the spawn cwd, not the cwd
        // verbatim — compute the same root the hook will so the assertion
        // is robust regardless of markers above the temp dir.
        let project = praca::project_root(&cwd);

        // Create the session through the daemon dispatch path with the
        // store wired in — the M1 hook fires.
        let sid = match dispatch_with_config(
            &inproc,
            &live,
            Request::NewSession {
                name: "praca-bind".into(),
                shell: "/bin/sh".into(),
                source: None,
                size_cells: None,
                args: Vec::new(),
            },
            None,
            Some(&store),
        ) {
            Response::SessionId(s) => s,
            other => panic!("expected SessionId, got {other:?}"),
        };

        // The binding is in the live store AND persisted to disk: a
        // fresh store at the same path (a "restart") sees it.
        store.with(|p| {
            assert_eq!(
                p.binding.lookup(&project),
                Some(sid),
                "live store bound the project root"
            );
        });
        assert!(path.exists(), "store persisted the binding to disk");

        let reloaded = crate::praca_store::PracaStore::open(path.clone());
        reloaded.with(|p| {
            assert_eq!(
                p.binding.lookup(&project),
                Some(sid),
                "binding survived a daemon restart"
            );
            let rec = p.index.get(sid).expect("record persisted");
            assert_eq!(rec.project_root, project);
            assert!(rec.visits >= 1, "frecency visit recorded");
        });

        let _ = inproc.kill_session(sid);
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn kill_session_via_dispatch_unbinds_in_persisted_store() {
        let path = praca_temp_path("kill-unbinds");
        let store = crate::praca_store::PracaStore::open(path.clone());
        let inproc = InProcess::new();
        let live = LiveConfig::default();
        let cwd = path.parent().unwrap().to_path_buf();
        inproc.set_spawn_env(
            tear_types::SpawnEnv::none().with_cwd(Some(cwd.to_string_lossy().into())),
        );
        let project = praca::project_root(&cwd);

        let sid = match dispatch_with_config(
            &inproc,
            &live,
            Request::NewSession {
                name: "praca-kill".into(),
                shell: "/bin/sh".into(),
                source: None,
                size_cells: None,
                args: Vec::new(),
            },
            None,
            Some(&store),
        ) {
            Response::SessionId(s) => s,
            other => panic!("expected SessionId, got {other:?}"),
        };
        store.with(|p| assert!(p.binding.lookup(&project).is_some()));

        // Kill it through the dispatch path — the unbind hook fires.
        match dispatch_with_config(
            &inproc,
            &live,
            Request::KillSession(sid),
            None,
            Some(&store),
        ) {
            Response::Ok => {}
            other => panic!("expected Ok, got {other:?}"),
        }

        // The binding + record are gone, in the live store and on disk.
        store.with(|p| {
            assert!(
                p.binding.lookup(&project).is_none(),
                "killed session's binding removed"
            );
            assert!(p.index.get(sid).is_none(), "killed session's record removed");
        });
        let reloaded = crate::praca_store::PracaStore::open(path.clone());
        reloaded.with(|p| assert!(p.binding.lookup(&project).is_none()));

        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn new_session_without_spawn_cwd_skips_binding() {
        // Bare-daemon path: no embedder cwd override → nothing to bind.
        let path = praca_temp_path("no-cwd");
        let store = crate::praca_store::PracaStore::open(path.clone());
        let inproc = InProcess::new();
        let live = LiveConfig::default();
        // Note: no set_spawn_env — spawn_cwd() is None.

        let _sid = match dispatch_with_config(
            &inproc,
            &live,
            Request::NewSession {
                name: "no-cwd".into(),
                shell: "/bin/sh".into(),
                source: None,
                size_cells: None,
                args: Vec::new(),
            },
            None,
            Some(&store),
        ) {
            Response::SessionId(s) => s,
            other => panic!("expected SessionId, got {other:?}"),
        };

        store.with(|p| {
            assert!(p.binding.is_empty(), "no spawn cwd → no binding");
            assert!(p.index.is_empty(), "no spawn cwd → no record");
        });

        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    // ── Capability negotiation ───────────────────────────────────

    /// Encode a request frame naming a variant this daemon's
    /// `Request` enum does not have — the exact shape of a client
    /// from a future vocabulary. Written structurally (externally
    /// tagged, field-name-keyed map) because that is literally what
    /// serde emits for a struct variant.
    #[cfg(test)]
    fn frame_for_an_unknown_variant(into: &mut Vec<u8>) {
        use ciborium::value::Value;
        let unknown = Value::Map(vec![(
            Value::Text("ThisVariantDoesNotExistYet".into()),
            Value::Map(vec![(Value::Text("k".into()), Value::Text("v".into()))]),
        )]);
        let mut payload = Vec::new();
        ciborium::ser::into_writer(&unknown, &mut payload).unwrap();
        into.extend_from_slice(&u32::try_from(payload.len()).unwrap().to_be_bytes());
        into.extend_from_slice(&payload);
    }

    /// **The class fix.** A request naming a variant this daemon has
    /// never heard of gets a legible refusal and the connection
    /// KEEPS SERVING — the very next request is answered normally.
    ///
    /// Before this, the read loop did `Err(e) => return Err(e)`, so
    /// the same frame closed the socket and the second request was
    /// never read. That made every future `Request` variant a flag
    /// day against any daemon left running.
    #[test]
    fn an_unknown_request_variant_is_refused_and_the_connection_survives() {
        use crate::testing::{drain_responses, DuplexStream};
        use std::sync::mpsc::channel;

        let mut input = Vec::new();
        frame_for_an_unknown_variant(&mut input);
        // The positive control: if the loop hung up, this is never
        // answered and the assertion below reads only one response.
        write_msg(&mut input, &Request::ListSessions).unwrap();

        let (tx, rx) = channel::<u8>();
        let stream = DuplexStream::new(input, tx);
        let inproc = Arc::new(InProcess::new());
        let live = Arc::new(LiveConfig::default());
        let _ = serve_connection_with_auth(stream, inproc, live, None, None);

        let resps = drain_responses(&rx);
        assert_eq!(
            resps.len(),
            2,
            "the connection must survive an undecodable frame; got: {resps:?}"
        );
        match &resps[0] {
            Response::Err(WireError::Rejected(msg)) => {
                assert!(
                    msg.contains("cannot decode that request"),
                    "refusal must say what happened, got: {msg}"
                );
                assert!(
                    msg.contains("unknown variant"),
                    "refusal must carry serde's own reason, got: {msg}"
                );
                assert!(
                    msg.contains(env!("CARGO_PKG_VERSION")),
                    "refusal must name the daemon build that could not decode it, got: {msg}"
                );
            }
            other => panic!("expected a Rejected refusal, got: {other:?}"),
        }
        assert!(
            matches!(resps[1], Response::Sessions(_)),
            "the next request must still be served; got: {:?}",
            resps[1]
        );
    }

    /// The refusal arm necessarily runs BEFORE the auth gate — we
    /// could not decode enough of the frame to know what was asked —
    /// so an UNAUTHENTICATED peer must not learn the daemon's version
    /// or the full list of variants it knows. It still gets refused,
    /// and the connection still survives; only the detail is withheld.
    #[test]
    fn an_unauthenticated_peer_is_refused_without_learning_the_daemons_shape() {
        use crate::testing::{drain_responses, DuplexStream};
        use std::sync::mpsc::channel;

        let mut input = Vec::new();
        frame_for_an_unknown_variant(&mut input);
        // Positive control: the connection must still be alive to
        // answer the auth exchange that follows.
        write_msg(&mut input, &Request::Authenticate("secret".into())).unwrap();
        write_msg(&mut input, &Request::ListSessions).unwrap();

        let (tx, rx) = channel::<u8>();
        let stream = DuplexStream::new(input, tx);
        let _ = serve_connection_with_auth(
            stream,
            Arc::new(InProcess::new()),
            Arc::new(LiveConfig::default()),
            None,
            Some("secret".into()),
        );

        let resps = drain_responses(&rx);
        assert_eq!(resps.len(), 3, "connection must survive; got: {resps:?}");
        match &resps[0] {
            Response::Err(WireError::Rejected(msg)) => {
                assert!(
                    !msg.contains(env!("CARGO_PKG_VERSION")),
                    "pre-auth refusal must not disclose the daemon version: {msg}"
                );
                assert!(
                    !msg.contains("unknown variant"),
                    "pre-auth refusal must not enumerate known variants: {msg}"
                );
                assert!(msg.contains("authenticate first"), "{msg}");
            }
            other => panic!("expected a Rejected refusal, got: {other:?}"),
        }
        assert!(matches!(resps[1], Response::Ok), "auth must still work");
        assert!(matches!(resps[2], Response::Sessions(_)));
    }

    /// The refusal is a REFUSAL, not an acceptance. A daemon that
    /// answered `Ok` to something it could not decode would be back
    /// to silent degradation with extra steps.
    #[test]
    fn an_unknown_request_variant_is_never_answered_ok() {
        use crate::testing::{drain_responses, DuplexStream};
        use std::sync::mpsc::channel;

        let mut input = Vec::new();
        frame_for_an_unknown_variant(&mut input);
        let (tx, rx) = channel::<u8>();
        let stream = DuplexStream::new(input, tx);
        let _ = serve_connection_with_auth(
            stream,
            Arc::new(InProcess::new()),
            Arc::new(LiveConfig::default()),
            None,
            None,
        );
        let resps = drain_responses(&rx);
        assert!(
            !resps.iter().any(|r| matches!(r, Response::Ok)),
            "an undecodable request must never read as success: {resps:?}"
        );
    }

    /// The capability probe answers with the DAEMON's version, not
    /// the caller's, and advertises `spawn-args` — the field whose
    /// silent drop started all of this.
    #[test]
    fn hello_answers_with_this_daemons_version_and_capabilities() {
        use crate::testing::{drain_responses, DuplexStream};
        use std::sync::mpsc::channel;

        let mut input = Vec::new();
        write_msg(
            &mut input,
            &Request::Hello {
                // Deliberately absurd, so a daemon echoing the
                // client's version back would be obvious.
                client_version: "99.99.99".into(),
            },
        )
        .unwrap();

        let (tx, rx) = channel::<u8>();
        let stream = DuplexStream::new(input, tx);
        let _ = serve_connection_with_auth(
            stream,
            Arc::new(InProcess::new()),
            Arc::new(LiveConfig::default()),
            None,
            None,
        );

        let resps = drain_responses(&rx);
        match resps.first() {
            Some(Response::Hello(h)) => {
                assert_eq!(h.daemon_version, env!("CARGO_PKG_VERSION"));
                assert_ne!(h.daemon_version, "99.99.99", "echoed the client's version");
                assert!(h.capabilities.contains(&"spawn-args".to_owned()));
            }
            other => panic!("expected Response::Hello, got: {other:?}"),
        }
    }

    /// On an auth-required daemon the probe is gated like everything
    /// else — a client must authenticate first. Pinned because the
    /// client's connect order (dial → authenticate → probe) depends
    /// on it.
    #[test]
    fn hello_before_authenticate_is_rejected_on_an_auth_required_daemon() {
        use crate::testing::{drain_responses, DuplexStream};
        use std::sync::mpsc::channel;

        let mut input = Vec::new();
        write_msg(
            &mut input,
            &Request::Hello {
                client_version: "0.0.0".into(),
            },
        )
        .unwrap();
        write_msg(&mut input, &Request::Authenticate("secret".into())).unwrap();
        write_msg(
            &mut input,
            &Request::Hello {
                client_version: "0.0.0".into(),
            },
        )
        .unwrap();

        let (tx, rx) = channel::<u8>();
        let stream = DuplexStream::new(input, tx);
        let _ = serve_connection_with_auth(
            stream,
            Arc::new(InProcess::new()),
            Arc::new(LiveConfig::default()),
            None,
            Some("secret".into()),
        );

        let resps = drain_responses(&rx);
        assert_eq!(resps.len(), 3, "got: {resps:?}");
        assert!(matches!(resps[0], Response::Err(WireError::Rejected(_))));
        assert!(matches!(resps[1], Response::Ok));
        assert!(matches!(resps[2], Response::Hello(_)));
    }

    /// `dispatch` and `dispatch_with_config` must agree about who
    /// the daemon is — a consumer driving the bare dispatcher must
    /// not see a different identity than one on the socket.
    #[test]
    fn both_dispatchers_report_the_same_daemon_identity() {
        let inproc = InProcess::new();
        let live = LiveConfig::default();
        let a = dispatch(
            &inproc,
            Request::Hello {
                client_version: String::new(),
            },
        );
        let b = dispatch_with_config(
            &inproc,
            &live,
            Request::Hello {
                client_version: String::new(),
            },
            None,
            None,
        );
        match (a, b) {
            (Response::Hello(x), Response::Hello(y)) => assert_eq!(x, y),
            (x, y) => panic!("expected two Hellos, got {x:?} / {y:?}"),
        }
    }
}
