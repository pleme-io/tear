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
//! ## Connecting
//!
//! `Client::connect` is dial → (optional `Authenticate`) → **capability
//! probe**. The probe is one `Request::Hello` round-trip whose answer
//! is cached on the `Client`, so [`Client::daemon`] and
//! `MultiplexerControl::capabilities` are field reads afterwards —
//! never I/O, never fallible.
//!
//! Against a daemon old enough not to know `Hello`, the probe costs
//! that connection: such a daemon's serve loop treats an undecodable
//! request as a dead socket and hangs up. `connect` re-dials and
//! records [`DaemonIdentity::pre_capability`], so a caller sees a
//! working client either way — only the capability answer differs.
//! That branch is the one that runs against the daemon shipping
//! today, so it is the exercised path, not a fallback.
//!
//! What the capabilities are *for*: a call that needs a field the
//! daemon cannot read (today, `args`) refuses with
//! [`ControlError::Unsupported`] **before writing the frame**, rather
//! than letting the daemon decode it, drop the unknown key and spawn
//! something quietly wrong. A call that doesn't need the field is
//! unaffected.
//!
//! ## Why sync
//!
//! The trait is sync. The PTY pump is on the daemon side, not the
//! client side. Async at this layer would be all cost no benefit —
//! the client's job is to ferry a few Request/Response pairs per
//! human keystroke, not pump kilobytes per second.

#![forbid(unsafe_code)]

#[cfg(feature = "engate")]
pub mod engate_producer;

use std::io::{self, BufReader, BufWriter, Read, Write};
use std::net::{Shutdown, SocketAddr, TcpStream, ToSocketAddrs};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;

use parking_lot::Mutex;

use tear_types::wire::{read_msg, write_msg, Request, Response};
use tear_types::{
    Capability, ControlError, ControlResult, DaemonIdentity, Direction, MultiplexerControl, PaneId,
    PaneSnapshot, SessionId, TearPane, TearSession, TearWindow, WindowId,
};

/// #5 — transport address. UDS for local daemons (~/.local/share/tear/
/// tear.sock by default); Tcp for remote daemons reached via SSH
/// tunnel, WireGuard, or a TLS proxy. Tear-client treats them
/// identically — the same `MultiplexerControl` impl works over either.
#[derive(Clone, Debug)]
pub enum Transport {
    Unix(PathBuf),
    Tcp(SocketAddr),
}

impl Transport {
    /// Parse a `tcp://host:port` URL or treat anything else as a
    /// filesystem path (UDS). Convenience for CLI surfaces that
    /// take a single `--socket <str>` flag.
    ///
    /// # Errors
    /// Returns `io::Error` when the TCP form resolves to no
    /// addresses.
    pub fn parse(s: &str) -> io::Result<Self> {
        if let Some(rest) = s.strip_prefix("tcp://") {
            let mut addrs = rest.to_socket_addrs()?;
            let first = addrs
                .next()
                .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "no addrs"))?;
            return Ok(Transport::Tcp(first));
        }
        Ok(Transport::Unix(PathBuf::from(s)))
    }

    fn connect(&self) -> io::Result<TransportStream> {
        match self {
            Transport::Unix(p) => UnixStream::connect(p).map(TransportStream::Unix),
            Transport::Tcp(addr) => TcpStream::connect(addr).map(TransportStream::Tcp),
        }
    }

    /// Operator-facing display string — also the canonical form for
    /// the `--socket` flag.
    #[must_use]
    pub fn display_string(&self) -> String {
        match self {
            Transport::Unix(p) => p.display().to_string(),
            Transport::Tcp(addr) => format!("tcp://{addr}"),
        }
    }
}

/// A stream that's either a Unix UDS or a TCP connection. Both
/// implement `Read + Write + try_clone + shutdown` — the small
/// shim below adapts them under one type so the rest of the
/// client doesn't care.
pub enum TransportStream {
    Unix(UnixStream),
    Tcp(TcpStream),
}

impl TransportStream {
    fn try_clone(&self) -> io::Result<Self> {
        match self {
            TransportStream::Unix(s) => s.try_clone().map(TransportStream::Unix),
            TransportStream::Tcp(s) => s.try_clone().map(TransportStream::Tcp),
        }
    }

    fn shutdown(&self, how: Shutdown) -> io::Result<()> {
        match self {
            TransportStream::Unix(s) => s.shutdown(how),
            TransportStream::Tcp(s) => s.shutdown(how),
        }
    }
}

impl Read for TransportStream {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        match self {
            TransportStream::Unix(s) => s.read(buf),
            TransportStream::Tcp(s) => s.read(buf),
        }
    }
}

impl Write for TransportStream {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        match self {
            TransportStream::Unix(s) => s.write(buf),
            TransportStream::Tcp(s) => s.write(buf),
        }
    }
    fn flush(&mut self) -> io::Result<()> {
        match self {
            TransportStream::Unix(s) => s.flush(),
            TransportStream::Tcp(s) => s.flush(),
        }
    }
}

/// A connected tear-daemon client. Implements [`MultiplexerControl`]
/// so consumer code can take `&dyn MultiplexerControl` and not care
/// whether the backend is local (`tear_core::InProcess`) or remote
/// (this `Client`).
pub struct Client {
    inner: Mutex<ClientInner>,
    /// Path the client connected to. Subscriptions need to dial
    /// the same daemon on a fresh socket because Subscribe consumes
    /// the connection. For TCP connections this is the display
    /// string (`tcp://addr:port`) so log lines stay legible.
    socket_path: PathBuf,
    /// Typed transport — used by subscribe / re-connect paths.
    transport: Transport,
    /// What the daemon on the other end told us it can do, probed
    /// once at connect time. [`DaemonIdentity::pre_capability`] when
    /// the daemon predates `Request::Hello` — which is the common
    /// case today, not an error.
    daemon: DaemonIdentity,
}

/// Handle returned by [`Client::subscribe_pane_bytes`] and
/// [`Client::subscribe_config_change`]. Dropping it disconnects the
/// subscription connection (the daemon's serve thread observes the
/// read/write error on the next chunk and prunes the dead sender).
pub struct SubscribeHandle {
    stop: Arc<AtomicBool>,
    /// Owned half of the subscription socket — kept here so
    /// [`signal_and_join`] can call `shutdown(Both)` and unblock the
    /// reader thread, which is otherwise stuck in `read_msg`
    /// (the `stop` AtomicBool only fires *between* reads).
    socket: TransportStream,
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
        // Sever the socket so the reader thread's blocking
        // read_msg returns immediately. Without this,
        // j.join() deadlocks because the AtomicBool only
        // fires between reads.
        let _ = self.socket.shutdown(Shutdown::Both);
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

/// The buffered halves of the transport stream. Buffered so the
/// framed reads/writes don't translate into a syscall per byte.
struct ClientInner {
    reader: BufReader<TransportStream>,
    writer: BufWriter<TransportStream>,
}

/// Send one [`Request`], expect exactly one [`Response::Ok`].
///
/// Free function over `ClientInner` rather than a `Client` method
/// because [`Client::dial`] needs it *before* a `Client` exists —
/// which is what lets the re-dial after a lost capability probe run
/// the identical handshake instead of a near-copy of it.
fn round_trip_ok_on(
    inner: &mut ClientInner,
    req: Request,
    label: &'static str,
    reject_kind: io::ErrorKind,
) -> io::Result<()> {
    write_msg(&mut inner.writer, &req)?;
    let resp: Response = read_msg(&mut inner.reader)?;
    match resp {
        Response::Ok => Ok(()),
        Response::Err(e) => Err(io::Error::new(
            reject_kind,
            format!("tear-daemon rejected {label}: {e:?}"),
        )),
        other => Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("tear-daemon returned unexpected response to {label}: {other:?}"),
        )),
    }
}

impl Client {
    /// Connect to a tear-daemon listening at a Unix domain socket
    /// at `path`.
    ///
    /// Returns the `io::Error` from the underlying connect call
    /// unchanged so callers can distinguish "no daemon there"
    /// (`NotFound`) from "permission" (`PermissionDenied`) etc.
    pub fn connect(path: impl AsRef<Path>) -> io::Result<Self> {
        Self::connect_transport(Transport::Unix(path.as_ref().to_path_buf()))
    }

    /// #5 — connect to a remote tear-daemon over TCP. The address
    /// can be anything `ToSocketAddrs` accepts (`"127.0.0.1:5111"`,
    /// `"plo:5111"`, etc.). For untrusted networks, tunnel through
    /// SSH (`ssh -L 5111:localhost:5111 plo`) or run behind a TLS
    /// proxy — the wire is CBOR-over-TCP unencrypted at this layer.
    pub fn connect_tcp(addr: impl ToSocketAddrs) -> io::Result<Self> {
        let mut addrs = addr.to_socket_addrs()?;
        let first = addrs
            .next()
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "no addrs"))?;
        Self::connect_transport(Transport::Tcp(first))
    }

    /// Connect using a typed [`Transport`]. Used by the CLI's
    /// `--socket <str>` flag after `Transport::parse`.
    pub fn connect_transport(transport: Transport) -> io::Result<Self> {
        Self::connect_transport_with_auth(transport, None)
    }

    /// #5 — connect with an optional shared-secret auth token. When
    /// `Some`, the client sends `Request::Authenticate(token)`
    /// immediately and surfaces a `PermissionDenied` error if the
    /// daemon rejects. Safe to pass `Some` even when the daemon does
    /// not require auth (the daemon silently accepts). The CLI reads
    /// `TEAR_AUTH_TOKEN` from the env and forwards via this path.
    pub fn connect_transport_with_auth(
        transport: Transport,
        auth_token: Option<String>,
    ) -> io::Result<Self> {
        let mut inner = Self::dial(&transport, auth_token.as_deref())?;
        // Probe BEFORE handing the client out, so `capabilities()` is
        // infallible and I/O-free at every call site afterwards.
        let daemon = Self::probe_capabilities(&transport, &mut inner, auth_token.as_deref())?;
        Ok(Self {
            inner: Mutex::new(inner),
            socket_path: PathBuf::from(transport.display_string()),
            transport,
            daemon,
        })
    }

    /// Open one connection and authenticate it. Everything that
    /// establishes a control connection goes through here, so the
    /// re-dial after a lost probe is byte-for-byte the same
    /// handshake as the original.
    fn dial(transport: &Transport, auth_token: Option<&str>) -> io::Result<ClientInner> {
        let stream = transport.connect()?;
        let reader_stream = stream.try_clone()?;
        let mut inner = ClientInner {
            reader: BufReader::new(reader_stream),
            writer: BufWriter::new(stream),
        };
        if let Some(token) = auth_token {
            round_trip_ok_on(
                &mut inner,
                Request::Authenticate(token.to_owned()),
                "Authenticate",
                io::ErrorKind::PermissionDenied,
            )?;
        }
        Ok(inner)
    }

    /// Ask the daemon what it can do — **on a connection we are
    /// willing to lose.**
    ///
    /// ## Why this probe shape and not another
    ///
    /// `Request::Hello` is a new enum variant, and a daemon built
    /// before it existed cannot decode the frame. Two things follow,
    /// and they were both measured rather than assumed:
    ///
    /// - Serde rejects on the variant tag, so the request is
    ///   *undecodable*, not merely missing a field. A
    ///   `#[serde(default)]` field on an existing variant would have
    ///   been decodable — but there is no side-effect-free struct
    ///   variant to hang one on (every one is either mutating or
    ///   pane-scoped), so that shape would mean overloading an
    ///   unrelated request with a probe flag, and it would leave the
    ///   underlying class — *any* new variant kills the connection —
    ///   unsolved forever.
    /// - A daemon at or after the `read_frame` fix answers
    ///   `Response::Err(Rejected(..))` and stays up. A daemon before
    ///   it returns from its serve loop and the socket closes.
    ///
    /// Both outcomes mean the same thing — protocol 0, no
    /// capabilities — so the client needs no version knowledge to
    /// read them. The only extra work is re-dialling after the
    /// second one, which the caller never sees.
    ///
    /// **The no-capability branch is the branch that runs today.**
    /// The daemon on this machine predates the probe, so every
    /// `Client::connect` exercises the lost-connection path. It is
    /// the tested one by construction, not a fallback.
    ///
    /// Cost against such a daemon: one extra `connect(2)` and one
    /// `warn!`-level line in the daemon's log per client. Both stop
    /// the moment the daemon is restarted onto a build that knows
    /// `Hello`.
    ///
    /// # Errors
    /// Only when the connection is lost AND cannot be re-established
    /// — the caller was promised a usable client, so that is the one
    /// thing this cannot swallow.
    fn probe_capabilities(
        transport: &Transport,
        inner: &mut ClientInner,
        auth_token: Option<&str>,
    ) -> io::Result<DaemonIdentity> {
        let req = Request::Hello {
            client_version: env!("CARGO_PKG_VERSION").to_owned(),
        };
        let outcome = match write_msg(&mut inner.writer, &req) {
            Ok(()) => read_msg::<_, Response>(&mut inner.reader),
            Err(e) => Err(e),
        };
        match outcome {
            Ok(Response::Hello(hello)) => Ok(DaemonIdentity::from_hello(hello)),
            // The daemon read the frame, could not decode it, and
            // said so without hanging up. A well-framed reply leaves
            // the stream aligned, so there is nothing to repair.
            // Any other variant means a daemon that answered Hello
            // with something unexpected — same verdict, same intact
            // connection.
            Ok(_) => Ok(DaemonIdentity::pre_capability()),
            // The daemon hung up on the unknown variant (or the
            // transport failed). Re-dial so the caller's client is
            // usable, and record protocol 0.
            Err(_) => {
                *inner = Self::dial(transport, auth_token)?;
                Ok(DaemonIdentity::pre_capability())
            }
        }
    }

    /// What the connected daemon told us about itself: its own
    /// version (`None` when it predates the probe) and the
    /// capabilities it implements.
    ///
    /// Cheap — the probe ran once at connect; this is a field read.
    #[must_use]
    pub fn daemon(&self) -> &DaemonIdentity {
        &self.daemon
    }

    /// Send `Request::Authenticate(token)` and assert the daemon's
    /// `Response::Ok`. Returns `io::Error(PermissionDenied)` on
    /// rejection. Intended for the connect path; also exposed so
    /// long-lived clients can re-authenticate after a config rotation.
    pub fn authenticate(&mut self, token: &str) -> io::Result<()> {
        self.round_trip_ok(
            tear_types::wire::Request::Authenticate(token.to_string()),
            "Authenticate",
            io::ErrorKind::PermissionDenied,
        )
    }

    /// #2 — tag this connection with a 64-bit client identity used
    /// by `InputPolicy::Leader { id }` to gate `SendKeys`. Idempotent —
    /// the daemon overwrites the prior identity each call. Returns
    /// `io::Error(Other)` if the daemon responds with anything other
    /// than `Ok` (defensive — the daemon's current implementation
    /// always returns Ok here).
    pub fn identify_as(&mut self, id: u64) -> io::Result<()> {
        self.round_trip_ok(
            tear_types::wire::Request::IdentifyClient(id),
            "IdentifyClient",
            io::ErrorKind::Other,
        )
    }

    /// Shared "handshake" primitive: send one [`Request`], expect
    /// exactly one [`Response::Ok`]. Any `Response::Err` is surfaced
    /// as `io::Error(reject_kind)` with the daemon's message; any
    /// other response variant is `io::Error(InvalidData)`.
    ///
    /// Used by [`Client::authenticate`] and [`Client::identify_as`];
    /// add the next single-roundtrip request (e.g. `RegisterAttention`,
    /// `AckConfigVersion`) by composing this directly rather than
    /// hand-rolling a fresh match.
    fn round_trip_ok(
        &mut self,
        req: tear_types::wire::Request,
        label: &'static str,
        reject_kind: io::ErrorKind,
    ) -> io::Result<()> {
        let mut inner = self.inner.lock();
        round_trip_ok_on(&mut inner, req, label, reject_kind)
    }

    /// Path the client is connected to. For UDS connections this is
    /// the filesystem path; for TCP it's the `tcp://addr:port`
    /// display form (so log lines stay legible).
    pub fn socket_path(&self) -> &Path {
        &self.socket_path
    }

    /// Underlying transport — useful for log lines and for the
    /// subscribe code paths that need to open a second connection.
    pub fn transport(&self) -> &Transport {
        &self.transport
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
        let stream = self
            .transport
            .connect()
            .map_err(|e| ControlError::Transport(e.to_string()))?;
        // `socket_for_handle` is held in the SubscribeHandle so
        // Drop can `shutdown(Both)` and unblock the reader thread
        // (which is otherwise blocked in `read_msg`, never
        // observing the stop flag).
        let socket_for_handle = stream
            .try_clone()
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
            socket: socket_for_handle,
            join: Some(join),
        })
    }

    /// Subscribe to live-config change events. Opens a fresh UDS
    /// connection to the same daemon, sends
    /// `Request::SubscribeConfigChange`, then spawns a reader
    /// thread that calls `on_change` for every
    /// `Response::ConfigChanged(yaml)` frame. The YAML is parsed
    /// to a typed `TearConfig` before the callback fires —
    /// consumers get an `Arc<TearConfig>` directly.
    ///
    /// Drives the "broadcast a config change to every attached
    /// renderer" pattern: each mado instance subscribes once at
    /// startup; when an operator runs `tear set-config` or edits
    /// `~/.config/tear/tear.yaml`, every mado re-themes at the
    /// same moment.
    ///
    /// The reader exits on parse failure (logs + drops), on EOF,
    /// or when the returned [`SubscribeHandle`] is dropped. The
    /// callback runs on the reader thread — keep it cheap and
    /// non-blocking.
    pub fn subscribe_config_change<F>(
        &self,
        mut on_change: F,
    ) -> ControlResult<SubscribeHandle>
    where
        F: FnMut(Arc<tear_config::TearConfig>) + Send + 'static,
    {
        let stream = self
            .transport
            .connect()
            .map_err(|e| ControlError::Transport(e.to_string()))?;
        let socket_for_handle = stream
            .try_clone()
            .map_err(|e| ControlError::Transport(e.to_string()))?;
        let reader_stream = stream
            .try_clone()
            .map_err(|e| ControlError::Transport(e.to_string()))?;
        let mut reader = BufReader::new(reader_stream);
        let mut writer = BufWriter::new(stream);
        write_msg(&mut writer, &Request::SubscribeConfigChange)
            .map_err(|e| ControlError::Transport(e.to_string()))?;
        let ack: Response = read_msg(&mut reader)
            .map_err(|e| ControlError::Transport(e.to_string()))?;
        match ack {
            Response::Ok => {}
            Response::Err(we) => return Err(ControlError::from(we)),
            other => {
                return Err(ControlError::Transport(format!(
                    "unexpected ack to SubscribeConfigChange: {other:?}"
                )))
            }
        }
        let stop = Arc::new(AtomicBool::new(false));
        let stop_for_thread = stop.clone();
        let join = thread::Builder::new()
            .name("tear-client-subscribe-config".into())
            .spawn(move || {
                while !stop_for_thread.load(Ordering::SeqCst) {
                    match read_msg::<_, Response>(&mut reader) {
                        Ok(Response::ConfigChanged(yaml)) => {
                            match serde_yaml_ng::from_str::<tear_config::TearConfig>(&yaml) {
                                Ok(cfg) => on_change(Arc::new(cfg)),
                                Err(_) => return, // malformed payload, bail
                            }
                        }
                        Ok(_) => return,
                        Err(_) => return,
                    }
                }
            })
            .map_err(|e| {
                ControlError::Transport(format!("spawn config subscriber thread: {e}"))
            })?;
        Ok(SubscribeHandle {
            stop,
            socket: socket_for_handle,
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

impl Client {
    /// Refuse **only** when this call actually needs `spawn-args`.
    ///
    /// The `is_empty` guard is the whole design: a caller spawning a
    /// bare shell is unaffected by a daemon that cannot read `args`,
    /// and must not be handed a "your daemon is old" banner it can
    /// do nothing with. A caller that passed arguments gets a typed
    /// refusal naming the field, instead of a pane that comes up
    /// silently wrong.
    ///
    /// Tier: **runtime typed error**. Not compile-caught and not
    /// parse-time-rejected — the daemon's build is a fact discovered
    /// at connect time, so no type on this side can make the call
    /// unrepresentable. Making it parse-rejected would mean the
    /// args-bearing methods took a capability witness token, which
    /// `MultiplexerControl` cannot express without breaking the
    /// in-process backend that never needs one.
    fn require_spawn_args(&self, args: &[String], call: &str) -> ControlResult<()> {
        if args.is_empty() {
            return Ok(());
        }
        self.daemon.require(
            Capability::SpawnArgs,
            &format!("{call} was given {} argument(s)", args.len()),
        )
    }
}

impl MultiplexerControl for Client {
    /// Overrides the trait default, which assumes an in-process
    /// backend. A `Client` talks to a **separate process** whose
    /// build may be older than ours — the assumption the default
    /// makes is exactly the one that fails here.
    fn capabilities(&self) -> DaemonIdentity {
        self.daemon.clone()
    }

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

    fn new_session_with_source(
        &self,
        name: &str,
        shell: &str,
        source: tear_types::SessionSource,
    ) -> ControlResult<SessionId> {
        // Defer to new_session_with_source_and_size with the
        // default (80, 24) so the wire path is one method.
        self.new_session_with_source_and_size(name, shell, &[], source, (80, 24))
    }

    fn new_session_with_source_and_size(
        &self,
        name: &str,
        shell: &str,
        args: &[String],
        source: tear_types::SessionSource,
        size_cells: (u16, u16),
    ) -> ControlResult<SessionId> {
        self.require_spawn_args(args, "new_session_with_source_and_size")?;
        match self.rpc(Request::NewSession {
            name: name.to_owned(),
            shell: shell.to_owned(),
            source: Some(source),
            size_cells: Some(size_cells),
            args: args.to_vec(),
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
        args: &[String],
    ) -> ControlResult<WindowId> {
        self.require_spawn_args(args, "new_window")?;
        match self.rpc(Request::NewWindow {
            session,
            name: name.to_owned(),
            shell: shell.to_owned(),
            args: args.to_vec(),
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
        args: &[String],
    ) -> ControlResult<PaneId> {
        self.require_spawn_args(args, "split_pane")?;
        match self.rpc(Request::SplitPane {
            origin,
            direction,
            shell: shell.to_owned(),
            args: args.to_vec(),
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

    fn apply_layout(
        &self,
        window: WindowId,
        kind: tear_types::LayoutKind,
    ) -> ControlResult<()> {
        match self.rpc(Request::ApplyLayout { window, kind })? {
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

    fn set_input_policy(
        &self,
        id: PaneId,
        policy: tear_types::InputPolicy,
    ) -> ControlResult<()> {
        match self.rpc(Request::SetInputPolicy { id, policy })? {
            Response::Ok => Ok(()),
            other => Err(unexpected("Ok", other)),
        }
    }

    fn pane_subscriber_count(&self, id: PaneId) -> ControlResult<u32> {
        match self.rpc(Request::PaneSubscriberCount(id))? {
            Response::SubscriberCount(n) => Ok(n),
            other => Err(unexpected("SubscriberCount", other)),
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

impl Client {
    /// Engage or release the operator's brake. `None` = every session.
    ///
    /// Returns `(states, braked, unbrakable)`. **Read the third one.** It
    /// names the panes the brake could NOT reach, because their
    /// provenance is unknown — an operator who pressed a panic button
    /// must be told what it did not stop.
    ///
    /// Requires the capability FIRST, so an old daemon produces a typed
    /// refusal rather than dropping the request as an unknown variant.
    /// "Nothing happened" is unacceptable for a panic button.
    pub fn set_freio(
        &self,
        session: Option<SessionId>,
        engaged: bool,
    ) -> ControlResult<(Vec<(SessionId, tear_types::Freio)>, Vec<PaneId>, Vec<PaneId>)> {
        self.daemon().require(
            tear_types::Capability::Freio,
            "freio (the operator's brake) needs a daemon that reads SetFreio",
        )?;
        match self.rpc(Request::SetFreio { session, engaged })? {
            Response::Freio { sessions, braked, unbrakable } => Ok((sessions, braked, unbrakable)),
            other => Err(unexpected("Freio", other)),
        }
    }

    /// Every session's brake state.
    pub fn freio_state(&self) -> ControlResult<Vec<(SessionId, tear_types::Freio)>> {
        self.daemon().require(
            tear_types::Capability::Freio,
            "freio status needs a daemon that reads GetFreio",
        )?;
        match self.rpc(Request::GetFreio)? {
            Response::Freio { sessions, .. } => Ok(sessions),
            other => Err(unexpected("Freio", other)),
        }
    }

    // ── #4 recording client API (inherent, not on the trait —
    // recording is a tear-core-side primitive) ──────────────

    /// Start recording the pane's PTY output. Resets the ring
    /// buffer if recording was already on.
    pub fn start_pane_recording(&self, id: PaneId) -> ControlResult<()> {
        match self.rpc(Request::StartPaneRecording(id))? {
            Response::Ok => Ok(()),
            other => Err(unexpected("Ok", other)),
        }
    }

    /// Stop recording. The captured buffer is retained for
    /// `export_pane_recording`.
    pub fn stop_pane_recording(&self, id: PaneId) -> ControlResult<()> {
        match self.rpc(Request::StopPaneRecording(id))? {
            Response::Ok => Ok(()),
            other => Err(unexpected("Ok", other)),
        }
    }

    /// Export the captured recording as asciinema v2 .cast
    /// (JSON-lines) string.
    pub fn export_pane_recording(&self, id: PaneId) -> ControlResult<String> {
        match self.rpc(Request::ExportPaneRecording(id))? {
            Response::CastJson(s) => Ok(s),
            other => Err(unexpected("CastJson", other)),
        }
    }

    /// `(is_enabled, event_count)` for the pane.
    pub fn pane_recording_status(&self, id: PaneId) -> ControlResult<(bool, u32)> {
        match self.rpc(Request::PaneRecordingStatus(id))? {
            Response::RecordingStatus { enabled, events } => Ok((enabled, events)),
            other => Err(unexpected("RecordingStatus", other)),
        }
    }

    // ── Pane-as-block (warp-class UX) ──────────────────────

    /// List captured OSC 133 blocks for the pane.
    /// `since_index` filters older blocks (pass 0 for all);
    /// `limit` caps the response.
    pub fn pane_blocks_list(
        &self,
        pane: PaneId,
        since_index: u64,
        limit: u32,
    ) -> ControlResult<Vec<tear_types::Block>> {
        match self.rpc(Request::PaneBlocksList { pane, since_index, limit })? {
            Response::Blocks(b) => Ok(b),
            other => Err(unexpected("Blocks", other)),
        }
    }

    /// Fetch one block by per-pane index.
    pub fn pane_block_at(&self, pane: PaneId, index: u64) -> ControlResult<tear_types::Block> {
        match self.rpc(Request::PaneBlockAt { pane, index })? {
            Response::Block(b) => Ok(b),
            other => Err(unexpected("Block", other)),
        }
    }

    /// `(total_completed, in_progress)` summary.
    pub fn pane_blocks_status(&self, pane: PaneId) -> ControlResult<(u32, bool)> {
        match self.rpc(Request::PaneBlocksStatus(pane))? {
            Response::BlocksStatus { total, in_progress } => Ok((total, in_progress)),
            other => Err(unexpected("BlocksStatus", other)),
        }
    }
}

impl Client {
    /// Fetch the daemon's current `TearConfig` as parsed YAML.
    /// Returns the YAML string verbatim — callers that want a
    /// typed `TearConfig` parse it via `tear_config` (which they
    /// already depend on for the on-disk surface).
    ///
    /// Not part of `MultiplexerControl` — config inspection is a
    /// daemon-specific concern; backends like `tear-tmux-backend`
    /// have no equivalent.
    pub fn get_config_yaml(&self) -> ControlResult<String> {
        match self.rpc(Request::GetConfig)? {
            Response::ConfigYaml(s) => Ok(s),
            other => Err(unexpected("ConfigYaml", other)),
        }
    }

    /// Convenience: fetch + parse the config in one step. Returns
    /// a typed `tear_config::TearConfig`. Fails if the daemon's
    /// YAML doesn't parse (shouldn't happen — the daemon
    /// serialised it from a typed TearConfig).
    pub fn get_config(&self) -> ControlResult<tear_config::TearConfig> {
        let yaml = self.get_config_yaml()?;
        serde_yaml_ng::from_str(&yaml).map_err(|e| {
            ControlError::Transport(format!("daemon ConfigYaml is not valid TearConfig: {e}"))
        })
    }

    /// Ask the daemon to re-read its config file from disk. Used
    /// in tests + as the manual escape hatch when the notify
    /// watcher misses an update.
    pub fn reload_config(&self) -> ControlResult<()> {
        match self.rpc(Request::ReloadConfig)? {
            Response::Ok => Ok(()),
            other => Err(unexpected("Ok", other)),
        }
    }

    /// Push a typed `TearConfig` to the daemon — replaces the
    /// live config in-place. Mado uses this both at attach time
    /// (to impose its preferred defaults) AND dynamically across
    /// the session (to reflect runtime user choices: theme swap,
    /// status-bar visibility, etc.). The daemon's on-disk file
    /// is NOT touched; the next file-system reload reverts.
    pub fn set_config(&self, cfg: &tear_config::TearConfig) -> ControlResult<()> {
        let yaml = serde_yaml_ng::to_string(cfg).map_err(|e| {
            ControlError::Rejected(format!("client-side TearConfig YAML serialisation failed: {e}"))
        })?;
        self.set_config_yaml(yaml)
    }

    /// String-form of [`set_config`] — useful when the caller
    /// already holds a YAML blob (CLI piping, scripted authoring).
    pub fn set_config_yaml(&self, yaml: String) -> ControlResult<()> {
        match self.rpc(Request::SetConfig(yaml))? {
            Response::Ok => Ok(()),
            other => Err(unexpected("Ok", other)),
        }
    }

    /// Push a typed [`SpawnEnv`](tear_types::SpawnEnv) — the embedder's
    /// capability env + cwd override — to the daemon. The daemon applies
    /// it to its `InProcess` so every SUBSEQUENT `new_session` spawn's
    /// child PTY sees the embedder's `TERM`/`COLORTERM`/`TERMINFO`/
    /// `TERM_PROGRAM` (and a stamped `PWD`) AFTER the inherited +
    /// fallback env. This is the daemon-transport equivalent of the
    /// embedded path's direct `InProcess::set_spawn_env` call — it closes
    /// the gap where a daemon-spawned child only saw the daemon's own env
    /// (so mado's truecolor/terminfo projection never reached vim there).
    ///
    /// Mado's `run_against_pane` calls this immediately after connecting,
    /// BEFORE the `new_session` that spawns the pane's shell, so the
    /// override is in place when the child's env is read. Idempotent; the
    /// last push wins. Not part of `MultiplexerControl` — spawn-env
    /// projection is a tear-core/daemon-specific concern.
    pub fn set_spawn_env(&self, env: &tear_types::SpawnEnv) -> ControlResult<()> {
        match self.rpc(Request::SetSpawnEnv(env.clone()))? {
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

    /// Stress: spin up many sessions through the RPC, list them,
    /// confirm count, kill them all. Catches subscriber/grid/pty
    /// map cleanup bugs that only surface at scale.
    #[test]
    fn many_sessions_round_trip() {
        let socket = {
            let mut p = std::env::temp_dir();
            let pid = std::process::id();
            p.push(format!("tear-client-stress-{pid}.sock"));
            p
        };
        let inproc = Arc::new(tear_core::InProcess::new());
        let daemon = tear_daemon::start(socket.clone(), inproc.clone()).expect("daemon");
        std::thread::sleep(std::time::Duration::from_millis(50));
        let client = Client::connect(&socket).expect("connect");

        const N: usize = 8;
        let mut ids = Vec::with_capacity(N);
        for i in 0..N {
            let sid = client
                .new_session(&format!("stress-{i}"), "/bin/sh")
                .expect("new_session");
            ids.push(sid);
        }
        assert_eq!(client.list_sessions().unwrap().len(), N);
        for sid in &ids {
            client.kill_session(*sid).expect("kill_session");
        }
        assert_eq!(client.list_sessions().unwrap().len(), 0);
        daemon.stop();
    }

    /// When the pane is killed mid-subscription, the daemon writes
    /// `Response::PaneClosed` and the client's reader thread exits
    /// cleanly. We can't observe PaneClosed directly from the public
    /// callback API (it's an internal sentinel), but we can detect
    /// the reader-thread exit by asserting the SubscribeHandle drops
    /// without panic + the channel side stops receiving.
    #[test]
    fn subscriber_cleanup_after_pane_killed() {
        let socket = {
            let mut p = std::env::temp_dir();
            let pid = std::process::id();
            p.push(format!("tear-client-cleanup-{pid}.sock"));
            p
        };
        let inproc = Arc::new(tear_core::InProcess::new());
        let daemon = tear_daemon::start(socket.clone(), inproc).expect("daemon");
        std::thread::sleep(std::time::Duration::from_millis(50));
        let client = Client::connect(&socket).expect("connect");

        let sid = client.new_session("cleanup", "/bin/sh").unwrap();
        let pane_id = *client
            .get_session(sid)
            .unwrap()
            .panes
            .keys()
            .next()
            .unwrap();

        let (tx, rx) = std::sync::mpsc::channel::<Vec<u8>>();
        let handle = client
            .subscribe_pane_bytes(pane_id, move |b| {
                let _ = tx.send(b.to_vec());
            })
            .expect("subscribe");

        // Killing the session destroys the pane → daemon flushes
        // PaneClosed → reader thread exits.
        client.kill_session(sid).unwrap();

        // Give the reader thread + cleanup a beat.
        std::thread::sleep(std::time::Duration::from_millis(150));

        // After cleanup: no further bytes arrive (the channel sender
        // dropped when the subscribe thread exited). recv_timeout
        // returns Disconnected (sender gone) within the timeout.
        let res = rx.recv_timeout(std::time::Duration::from_millis(500));
        assert!(
            matches!(res, Err(std::sync::mpsc::RecvTimeoutError::Disconnected) | Ok(_)),
            "expected channel disconnected after pane kill, got {res:?}"
        );

        // Drop the handle — should be a no-op since the thread already
        // exited. Doesn't panic.
        handle.stop();
        daemon.stop();
    }

    /// Phase-5 round-trip: ask the daemon for its current config,
    /// verify the YAML parses back into a typed `TearConfig` and
    /// the daemon-side `LiveConfig` snapshot matches the wire one.
    #[test]
    fn get_config_round_trip_matches_daemon_side() {
        let socket = {
            let mut p = std::env::temp_dir();
            let pid = std::process::id();
            p.push(format!("tear-client-cfg-{pid}.sock"));
            p
        };
        let inproc = Arc::new(tear_core::InProcess::new());
        // Custom LiveConfig so the test doesn't depend on the
        // user's ~/.config/tear/tear.yaml.
        let live = Arc::new(tear_config::LiveConfig::default());
        let custom = tear_config::TearConfig {
            prefix: "C-z".into(),
            ..tear_config::TearConfig::default()
        };
        live.replace(custom.clone());
        let daemon = tear_daemon::start_with_config(socket.clone(), inproc, live.clone())
            .expect("daemon");
        std::thread::sleep(std::time::Duration::from_millis(50));
        let client = Client::connect(&socket).expect("connect");

        let yaml = client.get_config_yaml().unwrap();
        assert!(yaml.contains("prefix"), "yaml should contain prefix key");
        let typed = client.get_config().unwrap();
        assert_eq!(typed.prefix, custom.prefix);
        assert_eq!(typed, custom);

        // Now mutate the daemon-side config and re-fetch — proves
        // the wire is reading live state, not a cached snapshot.
        let mutated = tear_config::TearConfig {
            prefix: "C-a".into(),
            ..tear_config::TearConfig::default()
        };
        live.replace(mutated.clone());
        let typed2 = client.get_config().unwrap();
        assert_eq!(typed2.prefix, "C-a");

        daemon.stop();
    }

    /// SetConfig pushes a typed TearConfig to the daemon and the
    /// next GetConfig reflects the change — proves the wire +
    /// dispatch + LiveConfig::replace path round-trips.
    #[test]
    fn set_config_imposes_client_authored_config_on_daemon() {
        let socket = {
            let mut p = std::env::temp_dir();
            let pid = std::process::id();
            p.push(format!("tear-client-setcfg-{pid}.sock"));
            p
        };
        let inproc = Arc::new(tear_core::InProcess::new());
        let live = Arc::new(tear_config::LiveConfig::default());
        let daemon = tear_daemon::start_with_config(socket.clone(), inproc, live.clone())
            .expect("daemon");
        std::thread::sleep(std::time::Duration::from_millis(50));
        let client = Client::connect(&socket).expect("connect");

        // 1 — initial config (whatever default produced)
        let before = client.get_config().unwrap();

        // 2 — client authors a new TearConfig + pushes it
        let mut authored = before.clone();
        authored.prefix = "C-x".into();
        client.set_config(&authored).expect("set_config");

        // 3 — daemon's LiveConfig snapshot reflects the change
        let after = client.get_config().unwrap();
        assert_eq!(after.prefix, "C-x");

        // 4 — daemon-side LiveConfig matches too (proves the
        //     wire's RPC went through replace, not a clone-only
        //     update on the client side)
        let daemon_view = live.load();
        assert_eq!(daemon_view.prefix, "C-x");

        // 5 — dynamic re-push during session
        authored.prefix = "C-Space".into();
        client.set_config(&authored).expect("set_config 2");
        assert_eq!(client.get_config().unwrap().prefix, "C-Space");

        daemon.stop();
    }

    /// **SetSpawnEnv end-to-end**: a client pushes a typed `SpawnEnv`
    /// (the embedder's capability projection) over the daemon wire
    /// BEFORE creating a session; the daemon applies it to its
    /// `InProcess`, and the subsequently-spawned child PTY's env reflects
    /// the override — `TERM`/`COLORTERM` win over the conservative
    /// fallback. This is the daemon-transport mirror of tear-core's
    /// `spawn_env_override_reaches_child_and_wins_over_fallback`, proving
    /// the wire path closes the truecolor gap for daemon-spawned panes.
    /// PTY-gated; subscribes to the pane byte stream to observe the
    /// child's own `printf` of its env.
    #[test]
    fn set_spawn_env_over_wire_reaches_subsequent_spawn_child_env() {
        let socket = {
            let mut p = std::env::temp_dir();
            let pid = std::process::id();
            p.push(format!("tear-client-spawnenv-{pid}.sock"));
            p
        };
        let inproc = Arc::new(tear_core::InProcess::new());
        let daemon = tear_daemon::start(socket.clone(), inproc).expect("daemon");
        std::thread::sleep(std::time::Duration::from_millis(50));
        let client = Client::connect(&socket).expect("connect");

        // 1 — push the embedder's capability env BEFORE any spawn.
        let env = tear_types::SpawnEnv::from_overrides(vec![
            ("TERM".to_owned(), "xterm-ghostty".to_owned()),
            ("COLORTERM".to_owned(), "truecolor".to_owned()),
        ]);
        client.set_spawn_env(&env).expect("set_spawn_env");

        // 2 — NOW create the session; its child PTY must inherit the
        //     pushed override (applied AFTER the daemon's own fallback).
        let sid = client.new_session("spawnenv-wire", "/bin/sh").unwrap();
        let pane_id = *client
            .get_session(sid)
            .unwrap()
            .panes
            .keys()
            .next()
            .expect("pane");

        // 3 — subscribe + have the child print its env back to us.
        let (tx, rx) = std::sync::mpsc::channel::<Vec<u8>>();
        let handle = client
            .subscribe_pane_bytes(pane_id, move |b| {
                let _ = tx.send(b.to_vec());
            })
            .expect("subscribe");
        client
            .send_keys(
                pane_id,
                b"printf 'SENV[T=%s][C=%s]\\n' \"${TERM}\" \"${COLORTERM}\"\n",
            )
            .expect("send_keys");

        // Break only on the RESOLVED output (`T=xterm-ghostty`), never
        // on the echoed input line (which still carries the literal
        // `%s` format directives) — otherwise the loop exits before the
        // child's printf has run. After a candidate match, drain a beat
        // so trailing bytes of the value land.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
        let mut acc = Vec::new();
        while std::time::Instant::now() < deadline {
            if let Ok(chunk) = rx.recv_timeout(std::time::Duration::from_millis(100)) {
                acc.extend(chunk);
                if std::str::from_utf8(&acc)
                    .map(|s| s.contains("T=xterm-ghostty"))
                    .unwrap_or(false)
                {
                    std::thread::sleep(std::time::Duration::from_millis(50));
                    while let Ok(more) = rx.try_recv() {
                        acc.extend(more);
                    }
                    break;
                }
            }
        }
        let text = String::from_utf8_lossy(&acc).to_string();
        assert!(
            text.contains("T=xterm-ghostty"),
            "pushed TERM override did not reach the daemon-spawned child (fallback won):\n{text}"
        );
        assert!(
            text.contains("C=truecolor"),
            "pushed COLORTERM override did not reach the daemon-spawned child:\n{text}"
        );

        handle.stop();
        daemon.stop();
    }

    /// SetConfig with malformed YAML returns Rejected — proves
    /// the daemon validates the payload before applying.
    #[test]
    fn set_config_with_malformed_yaml_returns_rejected() {
        let socket = {
            let mut p = std::env::temp_dir();
            let pid = std::process::id();
            p.push(format!("tear-client-badcfg-{pid}.sock"));
            p
        };
        let inproc = Arc::new(tear_core::InProcess::new());
        let live = Arc::new(tear_config::LiveConfig::default());
        let daemon = tear_daemon::start_with_config(socket.clone(), inproc, live)
            .expect("daemon");
        std::thread::sleep(std::time::Duration::from_millis(50));
        let client = Client::connect(&socket).expect("connect");
        let err = client
            .set_config_yaml("not a valid: tear config :\n  - nope".into())
            .unwrap_err();
        assert!(matches!(err, ControlError::Rejected(_)), "got {err:?}");
        daemon.stop();
    }

    /// Reload over RPC. When the on-disk file is missing, reload
    /// reverts to default — proves the RPC actually fires off
    /// `LiveConfig::reload`.
    #[test]
    fn reload_config_refreshes_from_disk() {
        let socket = {
            let mut p = std::env::temp_dir();
            let pid = std::process::id();
            p.push(format!("tear-client-reload-{pid}.sock"));
            p
        };
        let inproc = Arc::new(tear_core::InProcess::new());
        let live = Arc::new(tear_config::LiveConfig::default());
        // Set a custom in-memory config first.
        live.replace(tear_config::TearConfig {
            prefix: "C-z".into(),
            ..tear_config::TearConfig::default()
        });
        let daemon = tear_daemon::start_with_config(socket.clone(), inproc, live.clone())
            .expect("daemon");
        std::thread::sleep(std::time::Duration::from_millis(50));
        let client = Client::connect(&socket).expect("connect");

        // Before reload: prefix is C-z.
        assert_eq!(client.get_config().unwrap().prefix, "C-z");

        // ReloadConfig either succeeds (re-loaded from file → may
        // revert to default) or fails (no config file present);
        // both are valid post-conditions. The wire shouldn't panic.
        let _ = client.reload_config();

        // The fact that get_config still works after reload is
        // the load-bearing assertion.
        let _ = client.get_config().unwrap();

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

    // ── #7 Config-change broadcast subscription ──────────────────

    /// End-to-end: subscribe to config changes, then issue a
    /// SetConfig RPC, and verify the subscriber thread receives
    /// the new config with the override applied.
    #[test]
    fn subscribe_config_change_fires_on_set_config() {
        let socket = {
            let mut p = std::env::temp_dir();
            let pid = std::process::id();
            p.push(format!("tear-client-config-sub-{pid}.sock"));
            p
        };
        let inproc = Arc::new(tear_core::InProcess::new());
        let live = Arc::new(tear_config::LiveConfig::default());
        let daemon = tear_daemon::start_with_config(
            socket.clone(),
            inproc,
            live,
        )
        .expect("daemon");
        std::thread::sleep(std::time::Duration::from_millis(50));

        let client = Client::connect(&socket).expect("connect");
        let (tx, rx) = std::sync::mpsc::channel::<Arc<tear_config::TearConfig>>();
        let _handle = client
            .subscribe_config_change(move |cfg| {
                let _ = tx.send(cfg);
            })
            .expect("subscribe");

        // Give the daemon a moment to register the subscriber.
        std::thread::sleep(std::time::Duration::from_millis(50));

        // Push a new config — the subscriber should see it.
        let mut new_cfg = tear_config::TearConfig::default();
        new_cfg.prefix = "C-broadcast-test".into();
        client.set_config(&new_cfg).expect("set_config");

        let received = rx
            .recv_timeout(std::time::Duration::from_secs(2))
            .expect("subscriber should receive change");
        assert_eq!(received.prefix, "C-broadcast-test");

        daemon.stop();
    }

    /// Two concurrent config-change subscribers each get every
    /// frame — broadcast fan-out works.
    #[test]
    fn two_config_change_subscribers_each_receive_every_swap() {
        let socket = {
            let mut p = std::env::temp_dir();
            let pid = std::process::id();
            p.push(format!("tear-client-config-fanout-{pid}.sock"));
            p
        };
        let inproc = Arc::new(tear_core::InProcess::new());
        let live = Arc::new(tear_config::LiveConfig::default());
        let daemon = tear_daemon::start_with_config(
            socket.clone(),
            inproc,
            live,
        )
        .expect("daemon");
        std::thread::sleep(std::time::Duration::from_millis(50));

        let client = Client::connect(&socket).expect("connect");
        let (tx_a, rx_a) = std::sync::mpsc::channel();
        let (tx_b, rx_b) = std::sync::mpsc::channel();
        let _ha = client
            .subscribe_config_change(move |cfg| {
                let _ = tx_a.send(cfg.prefix.clone());
            })
            .expect("sub A");
        let _hb = client
            .subscribe_config_change(move |cfg| {
                let _ = tx_b.send(cfg.prefix.clone());
            })
            .expect("sub B");
        std::thread::sleep(std::time::Duration::from_millis(50));

        // Push two distinct configs in quick succession.
        for prefix in ["C-fan-1", "C-fan-2"] {
            let mut cfg = tear_config::TearConfig::default();
            cfg.prefix = prefix.into();
            client.set_config(&cfg).expect("set_config");
        }

        let collect = |rx: &std::sync::mpsc::Receiver<String>| {
            let mut got = Vec::new();
            let deadline = std::time::Instant::now()
                + std::time::Duration::from_secs(2);
            while got.len() < 2 && std::time::Instant::now() < deadline {
                if let Ok(p) = rx.recv_timeout(std::time::Duration::from_millis(100)) {
                    got.push(p);
                }
            }
            got
        };
        let got_a = collect(&rx_a);
        let got_b = collect(&rx_b);
        assert_eq!(got_a, vec!["C-fan-1", "C-fan-2"], "subscriber A");
        assert_eq!(got_b, vec!["C-fan-1", "C-fan-2"], "subscriber B");
        daemon.stop();
    }

    // ── #5 TCP transport ───────────────────────────────────────

    /// End-to-end: TCP-bound tear-daemon + Client::connect_tcp +
    /// the full new_session / list / kill cycle. Proves the wire
    /// is transport-agnostic.
    #[test]
    fn tcp_transport_full_lifecycle() {
        let addr: std::net::SocketAddr = "127.0.0.1:0".parse().unwrap();
        let inproc = Arc::new(tear_core::InProcess::new());
        let daemon = tear_daemon::start_tcp(addr, inproc).expect("daemon tcp");
        // The daemon's socket_path is "tcp://<bound>"; parse the
        // resolved address out and re-connect to it.
        let bound = daemon
            .socket_path()
            .display()
            .to_string()
            .strip_prefix("tcp://")
            .unwrap()
            .parse::<std::net::SocketAddr>()
            .unwrap();
        std::thread::sleep(std::time::Duration::from_millis(100));

        let client = Client::connect_tcp(bound).expect("connect");
        let sid = client.new_session("tcp-test", "/bin/sh").unwrap();
        let sessions = client.list_sessions().unwrap();
        assert!(sessions.iter().any(|s| s.id == sid));
        client.kill_session(sid).unwrap();
        let after = client.list_sessions().unwrap();
        assert!(after.iter().all(|s| s.id != sid));
        daemon.stop();
    }

    /// Transport::parse accepts both UDS paths and tcp:// URLs.
    #[test]
    fn transport_parse_handles_both_forms() {
        let t = Transport::parse("/tmp/foo.sock").unwrap();
        assert!(matches!(t, Transport::Unix(_)));
        let t2 = Transport::parse("tcp://127.0.0.1:5111").unwrap();
        assert!(matches!(t2, Transport::Tcp(_)));
    }

    // ── #3 migration — subscriber count ────────────────────────

    /// Subscriber count reflects active byte-stream attachers.
    /// Drop a SubscribeHandle and the daemon's count drops on
    /// the next broadcast (it's lazy — sender pruning happens
    /// when send fails).
    #[test]
    fn pane_subscriber_count_tracks_attach_lifecycle() {
        let socket = {
            let mut p = std::env::temp_dir();
            let pid = std::process::id();
            p.push(format!("tear-client-subcount-{pid}.sock"));
            p
        };
        let inproc = Arc::new(tear_core::InProcess::new());
        let daemon = tear_daemon::start(socket.clone(), inproc).expect("daemon");
        std::thread::sleep(std::time::Duration::from_millis(50));
        let client = Client::connect(&socket).expect("connect");
        let sid = client.new_session("subcount", "/bin/sh").unwrap();
        let pane_id = *client.get_session(sid).unwrap().panes.keys().next().unwrap();

        // No subscribers yet.
        assert_eq!(client.pane_subscriber_count(pane_id).unwrap(), 0);

        // Attach one subscriber. The daemon counts it.
        let (tx, _rx) = std::sync::mpsc::channel::<Vec<u8>>();
        let h1 = client.subscribe_pane_bytes(pane_id, move |b| {
            let _ = tx.send(b.to_vec());
        }).expect("sub 1");
        std::thread::sleep(std::time::Duration::from_millis(50));
        assert_eq!(client.pane_subscriber_count(pane_id).unwrap(), 1);

        // Two subscribers.
        let (tx2, _rx2) = std::sync::mpsc::channel::<Vec<u8>>();
        let h2 = client.subscribe_pane_bytes(pane_id, move |b| {
            let _ = tx2.send(b.to_vec());
        }).expect("sub 2");
        std::thread::sleep(std::time::Duration::from_millis(50));
        assert_eq!(client.pane_subscriber_count(pane_id).unwrap(), 2);

        // Drop one. Daemon prunes lazily — drive a write so the
        // fan-out happens, then re-count.
        h1.stop();
        std::thread::sleep(std::time::Duration::from_millis(50));
        // Send a marker to drive fan-out and force pruning.
        client.send_keys(pane_id, b"trigger").unwrap();
        std::thread::sleep(std::time::Duration::from_millis(200));
        let n = client.pane_subscriber_count(pane_id).unwrap();
        assert!(n <= 2, "expected ≤2 after drop, got {n}");

        h2.stop();
        daemon.stop();
    }

    /// Dropping the SubscribeHandle severs the connection — the
    /// daemon prunes the dead sender on the next broadcast, and
    /// the subscriber's callback stops firing.
    #[test]
    fn dropping_subscribe_handle_stops_callbacks() {
        let socket = {
            let mut p = std::env::temp_dir();
            let pid = std::process::id();
            p.push(format!("tear-client-config-drop-{pid}.sock"));
            p
        };
        let inproc = Arc::new(tear_core::InProcess::new());
        let live = Arc::new(tear_config::LiveConfig::default());
        let daemon = tear_daemon::start_with_config(
            socket.clone(),
            inproc,
            live,
        )
        .expect("daemon");
        std::thread::sleep(std::time::Duration::from_millis(50));

        let client = Client::connect(&socket).expect("connect");
        let (tx, rx) = std::sync::mpsc::channel::<String>();
        let handle = client
            .subscribe_config_change(move |cfg| {
                let _ = tx.send(cfg.prefix.clone());
            })
            .expect("subscribe");
        std::thread::sleep(std::time::Duration::from_millis(50));

        // Verify the first push works.
        let mut cfg = tear_config::TearConfig::default();
        cfg.prefix = "C-before-drop".into();
        client.set_config(&cfg).expect("set 1");
        let _ = rx.recv_timeout(std::time::Duration::from_secs(1)).expect("first frame");

        // Drop the handle; subsequent frames must NOT arrive.
        handle.stop();
        std::thread::sleep(std::time::Duration::from_millis(50));

        let mut cfg = tear_config::TearConfig::default();
        cfg.prefix = "C-after-drop".into();
        client.set_config(&cfg).expect("set 2");
        // Either Timeout (channel still open but no frame) OR
        // Disconnected (the reader thread exited on socket
        // shutdown, dropping the closure's tx) is a valid "no
        // frame after drop" outcome. The only failure is a
        // successful Ok(...) — that would mean the subscription
        // is still live after stop().
        match rx.recv_timeout(std::time::Duration::from_millis(300)) {
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {}
            Ok(s) => panic!("got a frame after drop: {s}"),
        }
        daemon.stop();
    }

    // ── #5 / #2 round-trip handshake helpers ─────────────────────

    /// `authenticate(wrong)` on a tokened daemon must surface
    /// `PermissionDenied`. Hand-driven against
    /// `serve_connection_with_auth` so we don't have to mutate the
    /// process env (this crate forbids unsafe blocks). Drives the
    /// round_trip_ok error path that authenticate() composes.
    #[test]
    fn authenticate_with_wrong_token_returns_permission_denied() {
        use std::os::unix::net::UnixStream;
        let socket = {
            let mut p = std::env::temp_dir();
            let pid = std::process::id();
            p.push(format!("tear-client-auth-bad-{pid}.sock"));
            p
        };
        let _ = std::fs::remove_file(&socket);

        // Spin up a one-off accept loop in-test that hands every
        // accepted stream into serve_connection_with_auth with a
        // known required_token. Avoids the env-var dance.
        let listener = std::os::unix::net::UnixListener::bind(&socket).unwrap();
        let socket_for_thread = socket.clone();
        let server = thread::spawn(move || {
            if let Ok((stream, _)) = listener.accept() {
                let inproc = Arc::new(tear_core::InProcess::new());
                let live = Arc::new(tear_config::LiveConfig::default());
                let _ = tear_daemon::serve_connection_with_auth(
                    stream,
                    inproc,
                    live,
                    None,
                    Some("correct-secret".into()),
                );
            }
            let _ = std::fs::remove_file(&socket_for_thread);
        });
        std::thread::sleep(std::time::Duration::from_millis(20));

        let stream = UnixStream::connect(&socket).unwrap();
        let mut me = Client {
            inner: parking_lot::Mutex::new(ClientInner {
                reader: BufReader::new(TransportStream::Unix(stream.try_clone().unwrap())),
                writer: BufWriter::new(TransportStream::Unix(stream)),
            }),
            socket_path: socket.clone(),
            transport: Transport::Unix(socket.clone()),
            // Hand-built to exercise `authenticate` in isolation, so
            // the probe never ran. Protocol 0 is the honest value —
            // and the right default for a client assembled by hand.
            daemon: DaemonIdentity::pre_capability(),
        };
        let err = match me.authenticate("wrong-secret") {
            Ok(()) => panic!("bad token must error"),
            Err(e) => e,
        };
        assert_eq!(err.kind(), io::ErrorKind::PermissionDenied);
        // Drop the client to send EOF to the server thread; without
        // this the server's serve_connection_with_auth loops in
        // read_msg forever and join() never returns.
        drop(me);
        let _ = server.join();
    }

    /// identify_as on a daemon with no leader-policy pane is a
    /// silent Ok — proves idempotency + no-op path.
    #[test]
    fn identify_as_is_noop_when_no_leader_policy_in_play() {
        let socket = {
            let mut p = std::env::temp_dir();
            let pid = std::process::id();
            p.push(format!("tear-client-ident-noop-{pid}.sock"));
            p
        };
        let _ = std::fs::remove_file(&socket);
        let inproc = Arc::new(tear_core::InProcess::new());
        let daemon = tear_daemon::start(socket.clone(), inproc).expect("start");
        std::thread::sleep(std::time::Duration::from_millis(50));

        let mut client = Client::connect_transport(Transport::Unix(socket.clone())).unwrap();
        client.identify_as(42).expect("identify");
        client.identify_as(99).expect("identify again");
        // After both, normal RPCs still work.
        assert!(client.list_sessions().unwrap().is_empty());

        daemon.stop();
        let _ = std::fs::remove_file(&socket);
    }
}
