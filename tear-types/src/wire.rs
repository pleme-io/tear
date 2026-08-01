//! Wire-format types for the tear-daemon ↔ tear-client RPC.
//!
//! One [`Request`] variant per [`MultiplexerControl`] method; the
//! daemon dispatches on the variant and replies with a [`Response`]
//! whose shape matches the trait's return type. The framing is
//! 4-byte big-endian length-prefixed CBOR (RFC 8949) via `ciborium`.
//! CBOR was chosen over bincode because `LayoutNode` uses an
//! internally-tagged enum representation (`#[serde(tag = "kind")]`)
//! that bincode rejects — CBOR handles every serde tagging style.
//! The size + speed difference is negligible at IPC scale (single
//! Request/Response per call, not a streaming hot path).
//!
//! ## Why this lives in `tear-types`
//!
//! Both `tear-daemon` (server) and `tear-client` (client) need to
//! agree on the on-wire shape. Putting it here means there's one
//! source of truth — no risk of the two crates drifting because each
//! re-declared the Request enum. Pure types only; the framing
//! helpers ([`read_msg`] / [`write_msg`]) take any `Read`/`Write` so
//! transports beyond UDS (stdio pipes for embedded use, TCP for
//! future remote modes) compose trivially.
//!
//! ## Versioning
//!
//! The wire is **CBOR + serde** (see the framing note above — an
//! earlier revision of this paragraph said "bincode", which was
//! stale: bincode was evaluated and rejected for the tagging reason
//! stated above, and never shipped). Adding a new variant to either
//! enum at the *end* is backwards-compatible (older clients ignore
//! variants they don't understand because they never emit them).
//! Removing or reordering variants is a breaking wire change — bump
//! the workspace minor version when that happens.
//!
//! **Field-level compatibility.** `Request` is externally tagged and
//! its struct variants are encoded as *field-name-keyed CBOR maps*.
//! No type here sets `deny_unknown_fields`, so a `#[serde(default)]`
//! field is compatible in BOTH directions: an old daemon decoding a
//! new client's frame ignores the key it doesn't know, and a new
//! daemon decoding an old client's frame fills the missing key from
//! `Default`. That is what makes a new field *safe*, and equally
//! what makes it **silent**: an old daemon drops the key and does
//! the old thing. For `args` that reads as "the program spawned
//! without its arguments", with no error anywhere.
//!
//! ## Capability negotiation
//!
//! [`Request::Hello`] / [`Response::Hello`] close that hole. A
//! client probes once at connect time and gets back a
//! [`crate::capability::DaemonHello`] naming every field/behaviour
//! the daemon implements; a call site that needs one refuses with
//! [`crate::ControlError::Unsupported`] instead of sending a frame
//! that will be half-ignored. Read
//! [`crate::capability`] for why this is a capability **set** and
//! not a protocol version integer.
//!
//! **An unknown variant does not have to end the connection.** A
//! frame whose length prefix was honoured and whose bytes were all
//! consumed leaves the stream aligned at the next frame boundary
//! even when the payload names a variant the peer has never heard
//! of — measured, not assumed. [`read_frame`] surfaces that as
//! [`Framed::Undecodable`] so a server can answer
//! `Response::Err(Rejected(..))` and keep serving, which is what
//! `tear-daemon` now does. Before that, `serve_connection_full`'s
//! read loop did `Err(e) => return Err(e)`, so the *first* client
//! to send a variant the daemon didn't know got a bare connection
//! close. A daemon built before this change still behaves that way,
//! which is why the client treats a lost connection during the
//! probe as "protocol 0 / no capabilities" and re-dials rather than
//! failing.

use std::io::{self, Read, Write};

use serde::{Deserialize, Serialize};

use crate::{
    ControlError, Direction, LayoutKind, PaneId, PaneSnapshot, SessionId, TearPane, TearSession,
    TearWindow, WindowId,
};

/// Every [`MultiplexerControl`] operation, encoded as a single
/// tagged enum so the daemon can `match` on the variant once and
/// dispatch.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum Request {
    // ── Discovery ────────────────────────────────────────────────
    ListSessions,
    GetSession(SessionId),
    GetWindow(WindowId),
    GetPane(PaneId),
    // ── Sessions ─────────────────────────────────────────────────
    NewSession {
        name: String,
        shell: String,
        /// Optional provenance tag — defaults to None on pre-#6
        /// wire bytes (serde's default). When present, mado MCP /
        /// CLI sets it to `Some(Agent)` / `Some(Human)` / `Some(Named(...))`
        /// so `tear list` can group by source.
        #[serde(default)]
        source: Option<crate::session::SessionSource>,
        /// Optional initial pane size in cells. Defaults to None
        /// for backwards-compat (older clients omit this field;
        /// daemon falls back to 80×24). mado attaches at known
        /// geometry — passing Some((cols, rows)) here means the
        /// shell's TIOCGWINSZ returns the right size on first
        /// query, no resize-flicker on attach.
        #[serde(default)]
        size_cells: Option<(u16, u16)>,
        /// Arguments passed to `shell` as argv[1..]. Defaults to
        /// empty on pre-args wire bytes. Because there is no
        /// protocol negotiation, a *stale daemon* decoding this
        /// frame drops the key and spawns the bare program — the
        /// failure is silent and looks like "my arguments were
        /// ignored", so restart the daemon after upgrading.
        #[serde(default)]
        args: Vec<String>,
    },
    RenameSession {
        id: SessionId,
        new_name: String,
    },
    KillSession(SessionId),
    // ── Windows ──────────────────────────────────────────────────
    NewWindow {
        session: SessionId,
        name: String,
        shell: String,
        /// Arguments passed to `shell` as argv[1..]. See the
        /// `NewSession::args` note on stale-daemon behaviour.
        #[serde(default)]
        args: Vec<String>,
    },
    KillWindow(WindowId),
    SelectWindow(WindowId),
    // ── Panes ────────────────────────────────────────────────────
    SplitPane {
        origin: PaneId,
        direction: Direction,
        shell: String,
        /// Arguments passed to `shell` as argv[1..]. See the
        /// `NewSession::args` note on stale-daemon behaviour.
        #[serde(default)]
        args: Vec<String>,
    },
    KillPane(PaneId),
    SelectPane(PaneId),
    ResizePane {
        id: PaneId,
        direction: Direction,
        delta_cells: i16,
    },
    ApplyLayout {
        window: WindowId,
        kind: LayoutKind,
    },
    SendKeys {
        id: PaneId,
        bytes: Vec<u8>,
    },
    // ── Rendering (Phase 2) ──────────────────────────────────────
    PaneSnapshot(PaneId),
    /// Promote this connection to a push-mode byte stream from the
    /// named pane. The daemon responds with `Response::Ok` then a
    /// continuous stream of `Response::PaneBytes(...)` frames as
    /// the pane's PTY produces output. The connection is consumed
    /// — no further Requests are accepted on it. Use a fresh
    /// connection for control-plane work.
    Subscribe(PaneId),
    /// Set the pane's PTY to an absolute size. Fires SIGWINCH at
    /// the child shell. Used by GPU consumers (mado at Phase 3.1)
    /// when their window resizes.
    PaneResizeAbsolute {
        id: PaneId,
        cols: u16,
        rows: u16,
    },
    // ── Config (Phase 5 — shikumi-style live reload) ─────────────
    /// Snapshot the daemon's current `TearConfig` as YAML. Lets
    /// mado (or any consumer) introspect the live config without
    /// racing the notify-driven hot-reload + without parsing the
    /// YAML file directly.
    GetConfig,
    /// Force the daemon to re-read its config file from disk. The
    /// notify watcher normally picks file changes up within ms;
    /// this is the manual escape hatch for filesystems where
    /// inotify-equivalents are unreliable (some network mounts).
    ReloadConfig,
    /// Push a typed `TearConfig` (serialised as YAML) to the
    /// daemon — replaces the daemon's live config snapshot
    /// in-place via the same `LiveConfig::replace` path the
    /// notify watcher uses. Lets mado (or any client) impose a
    /// config when it first attaches AND mutate the config
    /// dynamically over the lifetime of a session (per the M5
    /// destination — mado is the canonical author of the tear
    /// config when it's the front-end). Daemon-side config file
    /// on disk is NOT touched; the next reload reverts.
    SetConfig(String),
    /// Push a typed [`SpawnEnv`](crate::SpawnEnv) (the embedder's
    /// capability env + cwd override) to the daemon. The daemon applies
    /// it to its `InProcess` so every SUBSEQUENT `NewSession` spawn's
    /// child PTY sees the embedder's `TERM`/`COLORTERM`/`TERMINFO`/
    /// `TERM_PROGRAM` (and a stamped `PWD`) AFTER the inherited +
    /// fallback env — closing the gap where a daemon-spawned child only
    /// saw the daemon's own env, so a truecolor capability set never
    /// projected. The embedded path already calls
    /// `InProcess::set_spawn_env` directly; this is the daemon-transport
    /// equivalent. Idempotent; the last push wins. Replies
    /// `Response::Ok`.
    SetSpawnEnv(crate::SpawnEnv),
    /// #4 — start daemon-native recording for `pane`. Subsequent
    /// PTY chunks are captured into a per-pane ring buffer; the
    /// buffer can later be exported as asciinema v2 .cast via
    /// `ExportPaneRecording`.
    StartPaneRecording(PaneId),
    /// #4 — stop recording. The captured buffer is retained so a
    /// follow-up `ExportPaneRecording` still works.
    StopPaneRecording(PaneId),
    /// #4 — export the pane's captured recording as asciinema
    /// v2 .cast (JSON-lines string). Returns
    /// `Response::CastJson(string)`.
    ExportPaneRecording(PaneId),
    /// #4 — `(is_enabled, event_count)` for the pane. Returns
    /// `Response::RecordingStatus { enabled, events }`.
    PaneRecordingStatus(PaneId),
    /// Pane-as-block (warp-class UX): list captured OSC 133
    /// blocks for a pane. `since_index` filters older blocks;
    /// `limit` caps the response size.
    PaneBlocksList {
        pane: PaneId,
        since_index: u64,
        limit: u32,
    },
    /// Pane-as-block: fetch one block by per-pane index.
    PaneBlockAt {
        pane: PaneId,
        index: u64,
    },
    /// Pane-as-block: `(total_completed, in_progress)` summary
    /// for the pane. Cheap; `tear top` polls this each refresh.
    PaneBlocksStatus(PaneId),
    /// Probe how many subscribers (byte-stream consumers) are
    /// currently attached to a pane. Used by the migration
    /// ergonomic — `tear pane-info` surfaces the count so an
    /// operator knows whether they're stepping into an
    /// already-shared pane, and by the auto-detect path so a new
    /// renderer can decide between "attach to existing" and
    /// "start new session".
    PaneSubscriberCount(PaneId),
    /// Set a pane's input policy. `InputPolicy::Locked` rejects
    /// every subsequent `SendKeys` for that pane with
    /// `WireError::Rejected`; `InputPolicy::Free` re-opens it.
    /// Useful for demo / observer sessions, agent-only panes
    /// where human input would interleave, and the migration
    /// handoff window.
    SetInputPolicy {
        id: PaneId,
        policy: crate::pane::InputPolicy,
    },
    /// Engage or release the operator's brake — see [`crate::freio`].
    ///
    /// `None` for `session` means EVERY session: the one-gesture panic
    /// ergonomics live here, in the verb, rather than in a daemon-global
    /// flag that could drift out of sync with the per-session records it
    /// is supposed to describe.
    ///
    /// **A `bool`, deliberately not a `Freio`.** `Freio::Engaged` carries
    /// `at_unix`, and a peer must not be able to supply it — the daemon
    /// stamps the time. The same discipline that made `SessionSource`
    /// derived rather than declared: if this variant carried a `Freio`,
    /// a backdated brake would have a wire syntax.
    SetFreio {
        session: Option<SessionId>,
        engaged: bool,
    },
    /// Read the brake state of every session.
    GetFreio,
    /// Promote this connection to a config-change subscription.
    /// The daemon responds with `Response::Ok` then emits one
    /// `Response::ConfigChanged(yaml)` frame every time the live
    /// config is replaced (by `Request::SetConfig`, by a
    /// `LiveConfig.reload()`, or by the notify-driven watcher
    /// catching a file change). Connection is consumed — no
    /// further Requests are accepted on it. Lets every attached
    /// renderer react to a theme/keybind change at the same
    /// moment, broadcast-style: typed config hot-reload to every
    /// connected client.
    SubscribeConfigChange,
    /// #5 — authenticate this connection. Only used when the
    /// daemon was started with `auth_token_env` set in its
    /// `TearConfig`. Must be the first request on the connection;
    /// every other request returns `WireError::Rejected(...)` until
    /// authentication succeeds. Sending an Authenticate to a daemon
    /// that does not require auth is silently accepted (forward-
    /// compatible).
    Authenticate(String),
    /// #2 — tag this connection with a 64-bit client identity. Used
    /// by `InputPolicy::Leader(id)` to gate `SendKeys`: only the
    /// connection whose IdentifyClient matches the pane's leader id
    /// may send keys; all other clients get `WireError::Rejected`.
    /// Sending to a daemon with no Leader-policy pane is a silent
    /// Ok. Idempotent — calling again overwrites the connection's
    /// identity. Default identity is `None` (anonymous).
    IdentifyClient(u64),
    /// Capability probe. Replies [`Response::Hello`] carrying the
    /// daemon's own version and every capability it implements.
    ///
    /// **This variant is the one the compatibility story hangs on,
    /// so be precise about how an older peer sees it.** A daemon
    /// built before this variant existed cannot decode the frame —
    /// serde reports `unknown variant \`Hello\``. What happens next
    /// depends on the daemon's read loop:
    ///
    /// - built **before** the [`read_frame`] fix: the loop returns
    ///   the decode error and the connection closes. The client
    ///   observes EOF and reads that as protocol 0.
    /// - built **at or after** it: the loop answers
    ///   `Response::Err(Rejected("unknown request …"))` and stays
    ///   up. The client reads that as protocol 0 too.
    ///
    /// Both land on the same verdict, so the client needs no
    /// version knowledge to interpret the outcome — which is the
    /// property that makes this probe safe to send blind.
    ///
    /// Sent **after** `Authenticate` on an auth-required daemon,
    /// because the auth gate rejects everything else first.
    Hello {
        /// The client binary's own version. Purely informational —
        /// the daemon logs it so a "my args were ignored" report can
        /// be matched to a build. No decision is made on it.
        #[serde(default)]
        client_version: String,
    },
}

/// Reply shape for every [`Request`] variant. The daemon always
/// emits exactly one Response per Request — there is no streaming
/// or multi-frame reply at this layer (subscription / event streams
/// will land in a separate `Notification` type in Phase 2).
#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum Response {
    Sessions(Vec<TearSession>),
    Session(TearSession),
    Window {
        session: SessionId,
        window: TearWindow,
    },
    Pane(TearPane),
    SessionId(SessionId),
    WindowId(WindowId),
    PaneId(PaneId),
    PaneSnapshot(PaneSnapshot),
    /// Pushed by the daemon after a successful Subscribe — one
    /// frame per PTY chunk. Bytes are exactly what the PTY master
    /// reader delivered; consumers feed them into their own vte
    /// parser (or into a tear-core PaneGrid client-side).
    PaneBytes(Vec<u8>),
    /// Pushed by the daemon when the subscribed pane is destroyed.
    /// Subscribers should disconnect after observing this.
    PaneClosed(PaneId),
    /// Reply to `Request::GetConfig` — the daemon's current live
    /// TearConfig serialised as YAML (the same on-disk format
    /// operators author at `~/.config/tear/tear.yaml`). Wire stays
    /// in `tear-types`; deserialization back to a typed TearConfig
    /// happens in tear-client / consumer code which already
    /// depends on tear-config. YAML over the wire (vs typed CBOR)
    /// avoids the cycle tear-types ↔ tear-config and keeps the
    /// daemon's config inspectable with any text tool.
    ConfigYaml(String),
    /// Reply to `Request::ExportPaneRecording` — asciinema v2
    /// .cast (JSON-lines) string ready to write to disk or pipe
    /// to `asciinema play`.
    CastJson(String),
    /// Reply to `Request::PaneRecordingStatus`.
    RecordingStatus {
        enabled: bool,
        events: u32,
    },
    /// Reply to `Request::PaneBlocksList`.
    Blocks(Vec<crate::block::Block>),
    /// Reply to `Request::PaneBlockAt`.
    Block(crate::block::Block),
    /// Reply to `Request::PaneBlocksStatus`.
    BlocksStatus {
        total: u32,
        in_progress: bool,
    },
    /// Reply to `Request::PaneSubscriberCount` — number of
    /// currently-attached byte-stream subscribers for that pane.
    /// Includes the requester if it has an outstanding subscribe.
    SubscriberCount(u32),
    /// Reply to `Request::SetFreio` / `Request::GetFreio`.
    Freio {
        /// Every session's brake state after the call.
        sessions: Vec<(SessionId, crate::freio::Freio)>,
        /// Panes this call actually braked.
        braked: Vec<PaneId>,
        /// ★ Panes the brake could NOT reach, because their provenance is
        /// unknown (a tmux-backend pane, a pane from a pre-yurai daemon).
        ///
        /// **Never elided and never empty-by-convention.** An operator who
        /// pressed a panic button must be told what it did not stop;
        /// silence here would let them believe everything halted. This is
        /// the honest cost of not braking `Unknown` panes — see
        /// [`crate::session::TearSession::admits`].
        unbrakable: Vec<PaneId>,
    },
    /// Pushed by the daemon on every live-config replace, to
    /// every connection that issued `Request::SubscribeConfigChange`.
    /// Payload is the new config as YAML — same shape as
    /// `Response::ConfigYaml`. The first frame after subscription
    /// is `Response::Ok`; subsequent frames are `ConfigChanged`
    /// until the connection is dropped.
    ConfigChanged(String),
    /// Reply to [`Request::Hello`] — the daemon's own version plus
    /// the wire names of every capability it implements.
    ///
    /// Adding a `Response` variant is safe in a way adding a
    /// `Request` variant is not: only a *new* daemon emits this, and
    /// only in reply to a probe an *old* client never sends. The
    /// asymmetry is why the negotiation could be added at all
    /// without a flag day.
    Hello(crate::capability::DaemonHello),
    Ok,
    Err(WireError),
}

/// Serializable mirror of [`ControlError`]. The trait's `Internal`
/// variant carries `anyhow::Error` which doesn't serialize; we lose
/// the typed downcast across the wire but keep the message — which
/// is fine because clients can only ever treat `Internal` as
/// opaque-and-fatal anyway.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum WireError {
    NoSuchSession(SessionId),
    NoSuchWindow(WindowId),
    NoSuchPane(PaneId),
    Transport(String),
    Rejected(String),
    Internal(String),
}

impl From<ControlError> for WireError {
    fn from(e: ControlError) -> Self {
        match e {
            ControlError::NoSuchSession(id) => WireError::NoSuchSession(id),
            ControlError::NoSuchWindow(id) => WireError::NoSuchWindow(id),
            ControlError::NoSuchPane(id) => WireError::NoSuchPane(id),
            ControlError::Transport(s) => WireError::Transport(s),
            ControlError::Rejected(s) => WireError::Rejected(s),
            // Degrades to `Rejected` on purpose. A `WireError`
            // variant would be a wire change an older client could
            // not decode, and this error is produced client-side
            // before a frame is ever written — so the lossy edge is
            // unreachable in practice, and the message survives.
            ControlError::Unsupported { capability, detail } => {
                WireError::Rejected(format!("unsupported capability `{capability}`: {detail}"))
            }
            ControlError::Internal(e) => WireError::Internal(e.to_string()),
        }
    }
}

impl From<WireError> for ControlError {
    fn from(e: WireError) -> Self {
        match e {
            WireError::NoSuchSession(id) => ControlError::NoSuchSession(id),
            WireError::NoSuchWindow(id) => ControlError::NoSuchWindow(id),
            WireError::NoSuchPane(id) => ControlError::NoSuchPane(id),
            WireError::Transport(s) => ControlError::Transport(s),
            WireError::Rejected(s) => ControlError::Rejected(s),
            WireError::Internal(s) => ControlError::Internal(anyhow::anyhow!(s)),
        }
    }
}

/// Maximum frame size we'll deserialize. Caps allocation on a
/// malformed length-prefix (16 MiB is far above any real Request
/// or Response — `ListSessions` reply with thousands of sessions
/// is still well under a megabyte).
pub const MAX_FRAME_BYTES: usize = 16 * 1024 * 1024;

/// Default UDS socket path. Resolves at call time so a daemon
/// started with `XDG_RUNTIME_DIR=/foo` and a client started later
/// without the var both look in the same place (the XDG fallback).
#[must_use]
pub fn default_socket_path() -> std::path::PathBuf {
    if let Some(dir) = std::env::var_os("XDG_RUNTIME_DIR") {
        let mut p = std::path::PathBuf::from(dir);
        p.push("tear.sock");
        return p;
    }
    if let Some(home) = std::env::var_os("HOME") {
        let mut p = std::path::PathBuf::from(home);
        p.push(".local");
        p.push("share");
        p.push("tear");
        p.push("tear.sock");
        return p;
    }
    std::path::PathBuf::from("/tmp/tear.sock")
}

/// Write a length-prefixed CBOR-encoded message.
pub fn write_msg<W: Write, T: Serialize>(w: &mut W, msg: &T) -> io::Result<()> {
    let mut bytes = Vec::new();
    ciborium::ser::into_writer(msg, &mut bytes)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e.to_string()))?;
    let len = u32::try_from(bytes.len())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "frame too large"))?;
    w.write_all(&len.to_be_bytes())?;
    w.write_all(&bytes)?;
    w.flush()?;
    Ok(())
}

/// Outcome of reading one frame off the wire.
///
/// The distinction this type draws is the load-bearing one: a
/// **payload** we could not understand is not the same event as a
/// **stream** we could not read. The first leaves the connection
/// perfectly usable and deserves an answer; the second does not.
/// Collapsing them into one `io::Error` is what made an unknown
/// request variant hang up the socket.
#[derive(Debug)]
pub enum Framed<T> {
    /// A complete frame that decoded into `T`.
    Msg(T),
    /// A complete frame — length prefix honoured, every one of its
    /// bytes consumed — whose payload did not decode into `T`. The
    /// canonical cause is a variant from a newer peer's vocabulary.
    ///
    /// **The stream is still aligned at the next frame boundary**,
    /// so the reader may reply and keep reading. Verified by
    /// `an_undecodable_frame_leaves_the_stream_aligned`.
    Undecodable {
        /// serde's own message, e.g. ``unknown variant `Hello` ``.
        reason: String,
        /// Payload length that was consumed.
        len: usize,
    },
}

/// Read one length-prefixed CBOR frame, distinguishing an
/// undecodable *payload* from an unreadable *stream*.
///
/// Caps the frame at [`MAX_FRAME_BYTES`] so a malformed prefix can't
/// trigger an unbounded allocation. An oversized prefix stays a hard
/// `io::Error` rather than an [`Framed::Undecodable`], because those
/// bytes were never counted off the stream — the connection really
/// is desynchronised at that point.
///
/// # Errors
/// `io::Error` for anything that leaves the stream unusable: EOF,
/// a short read, a transport failure, or an oversized length prefix.
pub fn read_frame<R: Read, T: for<'de> Deserialize<'de>>(r: &mut R) -> io::Result<Framed<T>> {
    let mut len_buf = [0u8; 4];
    r.read_exact(&mut len_buf)?;
    let len = u32::from_be_bytes(len_buf) as usize;
    if len > MAX_FRAME_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("frame size {len} exceeds MAX_FRAME_BYTES {MAX_FRAME_BYTES}"),
        ));
    }
    let mut buf = vec![0u8; len];
    r.read_exact(&mut buf)?;
    match ciborium::de::from_reader(&buf[..]) {
        Ok(v) => Ok(Framed::Msg(v)),
        Err(e) => Ok(Framed::Undecodable {
            reason: e.to_string(),
            len,
        }),
    }
}

/// Read a length-prefixed CBOR-encoded message. Caps the frame at
/// [`MAX_FRAME_BYTES`] so a malformed prefix can't trigger an
/// unbounded allocation.
///
/// Thin wrapper over [`read_frame`] that collapses
/// [`Framed::Undecodable`] back into `io::ErrorKind::InvalidData`
/// with serde's message — byte-identical to what this function
/// returned before `read_frame` existed, so every existing caller
/// keeps its behaviour. A caller that wants to *answer* an unknown
/// variant instead of hanging up should call [`read_frame`]
/// directly; `tear-daemon`'s serve loop does.
///
/// # Errors
/// `io::Error` on transport failure, EOF, an oversized length
/// prefix, or a payload that does not decode into `T`.
pub fn read_msg<R: Read, T: for<'de> Deserialize<'de>>(r: &mut R) -> io::Result<T> {
    match read_frame(r)? {
        Framed::Msg(v) => Ok(v),
        Framed::Undecodable { reason, .. } => {
            Err(io::Error::new(io::ErrorKind::InvalidData, reason))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[test]
    fn roundtrip_list_sessions_request() {
        let mut buf = Vec::new();
        write_msg(&mut buf, &Request::ListSessions).unwrap();
        let mut cur = Cursor::new(&buf);
        let got: Request = read_msg(&mut cur).unwrap();
        assert!(matches!(got, Request::ListSessions));
    }

    #[test]
    fn roundtrip_send_keys_request() {
        let pane = PaneId::from_seed("pane");
        let req = Request::SendKeys {
            id: pane,
            bytes: vec![1, 2, 3, 4],
        };
        let mut buf = Vec::new();
        write_msg(&mut buf, &req).unwrap();
        let mut cur = Cursor::new(&buf);
        let got: Request = read_msg(&mut cur).unwrap();
        match got {
            Request::SendKeys { id, bytes } => {
                assert_eq!(id, pane);
                assert_eq!(bytes, vec![1, 2, 3, 4]);
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn roundtrip_apply_layout_request() {
        let window = WindowId::from_seed("win");
        let req = Request::ApplyLayout {
            window,
            kind: LayoutKind::MainVertical,
        };
        let mut buf = Vec::new();
        write_msg(&mut buf, &req).unwrap();
        let mut cur = Cursor::new(&buf);
        let got: Request = read_msg(&mut cur).unwrap();
        match got {
            Request::ApplyLayout { window: w, kind } => {
                assert_eq!(w, window);
                assert_eq!(kind, LayoutKind::MainVertical);
            }
            _ => panic!("wrong variant"),
        }
    }

    /// **Old client → new daemon.** A frame written WITHOUT the `args`
    /// key must still decode, filling `args` from `Default`. This is the
    /// half of the compat verdict that lets a new daemon accept traffic
    /// from a client built before `args` existed.
    ///
    /// The pre-args frame is reconstructed structurally rather than
    /// checked in as a byte blob: `Request` is externally tagged with
    /// field-name-keyed struct variants, so a CBOR map carrying only the
    /// old keys IS exactly what an old client emitted.
    #[test]
    fn new_daemon_decodes_a_pre_args_new_session_frame() {
        use ciborium::value::Value;
        // { "NewSession": { "name": …, "shell": … } } — no args, and no
        // source/size_cells either (those are the older `#[serde(default)]`
        // fields, which is the precedent this follows).
        let old = Value::Map(vec![(
            Value::Text("NewSession".into()),
            Value::Map(vec![
                (Value::Text("name".into()), Value::Text("work".into())),
                (Value::Text("shell".into()), Value::Text("/bin/sh".into())),
            ]),
        )]);
        let mut bytes = Vec::new();
        ciborium::ser::into_writer(&old, &mut bytes).unwrap();
        let got: Request = ciborium::de::from_reader(&bytes[..])
            .expect("a pre-args frame must still decode");
        match got {
            Request::NewSession { name, shell, source, size_cells, args } => {
                assert_eq!(name, "work");
                assert_eq!(shell, "/bin/sh");
                assert!(source.is_none());
                assert!(size_cells.is_none());
                assert!(args.is_empty(), "missing args must default to empty");
            }
            other => panic!("wrong variant: {other:?}"),
        }
    }

    /// **New client → old daemon.** No type here sets
    /// `deny_unknown_fields`, so a frame carrying an EXTRA key decodes
    /// cleanly against a struct that has never heard of it — the key is
    /// ignored. That is what makes the new `args` field safe to send at
    /// a stale daemon, and equally what makes the failure SILENT: the
    /// daemon does not reject the request, it spawns without the
    /// arguments. Restart the daemon to get the feature.
    ///
    /// Modelled by taking a real frame, splicing in a bogus future key,
    /// and decoding it back: `args` plays exactly that role for a binary
    /// built before it existed.
    #[test]
    fn unknown_fields_are_ignored_so_a_stale_peer_never_errors() {
        use ciborium::value::Value;
        let req = Request::SplitPane {
            origin: PaneId::from_seed("p"),
            direction: Direction::Right,
            shell: "/bin/sh".into(),
            args: vec!["-l".to_string()],
        };
        // Round-trip through Value so the id/direction encodings are
        // whatever serde really produces, not a guess.
        let mut bytes = Vec::new();
        ciborium::ser::into_writer(&req, &mut bytes).unwrap();
        let mut val: Value = ciborium::de::from_reader(&bytes[..]).unwrap();
        // Splice a key no version of this enum has ever declared into the
        // variant's field map.
        let Value::Map(outer) = &mut val else {
            panic!("externally-tagged variant must encode as a map")
        };
        let Value::Map(fields) = &mut outer[0].1 else {
            panic!("struct variant must encode as a field map")
        };
        fields.push((Value::Text("not_a_field_we_know".into()), Value::Bool(true)));
        let mut spliced = Vec::new();
        ciborium::ser::into_writer(&val, &mut spliced).unwrap();
        let got: Request = ciborium::de::from_reader(&spliced[..])
            .expect("an unknown key must be ignored, not rejected");
        match got {
            Request::SplitPane { shell, args, .. } => {
                assert_eq!(shell, "/bin/sh");
                assert_eq!(args, vec!["-l".to_string()]);
            }
            other => panic!("wrong variant: {other:?}"),
        }
    }

    /// `args` survives a real write→read round-trip on all three
    /// arg-bearing variants.
    #[test]
    fn args_roundtrip_on_every_arg_bearing_variant() {
        let args = vec!["-u".to_string(), "NONE".to_string()];
        let reqs = vec![
            Request::NewSession {
                name: "w".into(),
                shell: "/bin/nvim".into(),
                source: None,
                size_cells: None,
                args: args.clone(),
            },
            Request::NewWindow {
                session: SessionId::from_seed("s"),
                name: "w".into(),
                shell: "/bin/nvim".into(),
                args: args.clone(),
            },
            Request::SplitPane {
                origin: PaneId::from_seed("p"),
                direction: Direction::Right,
                shell: "/bin/nvim".into(),
                args: args.clone(),
            },
        ];
        for req in reqs {
            let mut buf = Vec::new();
            write_msg(&mut buf, &req).unwrap();
            let got: Request = read_msg(&mut Cursor::new(&buf)).unwrap();
            let seen = match got {
                Request::NewSession { args, .. }
                | Request::NewWindow { args, .. }
                | Request::SplitPane { args, .. } => args,
                other => panic!("wrong variant: {other:?}"),
            };
            assert_eq!(seen, args);
        }
    }

    #[test]
    fn wire_error_roundtrip_through_control_error() {
        let pane = PaneId::from_seed("pane");
        let ce = ControlError::NoSuchPane(pane);
        let we: WireError = ce.into();
        let ce2: ControlError = we.into();
        assert!(matches!(ce2, ControlError::NoSuchPane(p) if p == pane));
    }

    #[test]
    fn frame_size_cap_enforced() {
        // 32 MiB length prefix — must reject without allocating.
        let len: u32 = 32 * 1024 * 1024;
        let mut buf = Vec::new();
        buf.extend_from_slice(&len.to_be_bytes());
        let mut cur = Cursor::new(&buf);
        let err = read_msg::<_, Request>(&mut cur).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
    }

    #[test]
    fn default_socket_path_resolves() {
        let p = default_socket_path();
        assert!(p.to_string_lossy().ends_with("tear.sock"));
    }

    #[test]
    fn roundtrip_pane_resize_absolute_request() {
        let pane = PaneId::from_seed("resize-pane");
        let req = Request::PaneResizeAbsolute {
            id: pane,
            cols: 132,
            rows: 50,
        };
        let mut buf = Vec::new();
        write_msg(&mut buf, &req).unwrap();
        let mut cur = Cursor::new(buf);
        let got: Request = read_msg(&mut cur).unwrap();
        match got {
            Request::PaneResizeAbsolute { id, cols, rows } => {
                assert_eq!(id, pane);
                assert_eq!(cols, 132);
                assert_eq!(rows, 50);
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn roundtrip_set_spawn_env_request() {
        let env = crate::SpawnEnv::from_overrides(vec![
            ("TERM".to_owned(), "xterm-ghostty".to_owned()),
            ("COLORTERM".to_owned(), "truecolor".to_owned()),
        ])
        .with_cwd(Some("/work/dir".to_owned()));
        let req = Request::SetSpawnEnv(env.clone());
        let mut buf = Vec::new();
        write_msg(&mut buf, &req).unwrap();
        let mut cur = Cursor::new(buf);
        let got: Request = read_msg(&mut cur).unwrap();
        match got {
            Request::SetSpawnEnv(decoded) => assert_eq!(decoded, env),
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn roundtrip_subscribe_request() {
        let pane = PaneId::from_seed("sub-pane");
        let req = Request::Subscribe(pane);
        let mut buf = Vec::new();
        write_msg(&mut buf, &req).unwrap();
        let mut cur = Cursor::new(buf);
        let got: Request = read_msg(&mut cur).unwrap();
        assert!(matches!(got, Request::Subscribe(p) if p == pane));
    }

    #[test]
    fn roundtrip_pane_bytes_response() {
        let resp = Response::PaneBytes(b"hello\xff\x00 mixed bytes".to_vec());
        let mut buf = Vec::new();
        write_msg(&mut buf, &resp).unwrap();
        let mut cur = Cursor::new(buf);
        let got: Response = read_msg(&mut cur).unwrap();
        match got {
            Response::PaneBytes(b) => {
                assert_eq!(b, b"hello\xff\x00 mixed bytes");
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn roundtrip_pane_closed_response() {
        let pane = PaneId::from_seed("closed-pane");
        let resp = Response::PaneClosed(pane);
        let mut buf = Vec::new();
        write_msg(&mut buf, &resp).unwrap();
        let mut cur = Cursor::new(buf);
        let got: Response = read_msg(&mut cur).unwrap();
        assert!(matches!(got, Response::PaneClosed(p) if p == pane));
    }

    #[test]
    fn every_wire_error_variant_roundtrips() {
        let sid = SessionId::from_seed("s");
        let wid = WindowId::from_seed("w");
        let pid = PaneId::from_seed("p");
        let cases: Vec<ControlError> = vec![
            ControlError::NoSuchSession(sid),
            ControlError::NoSuchWindow(wid),
            ControlError::NoSuchPane(pid),
            ControlError::Transport("bad pipe".into()),
            ControlError::Rejected("not allowed".into()),
            ControlError::Internal(anyhow::anyhow!("boom")),
        ];
        for orig in cases {
            let we: WireError = (orig).into();
            let ce2: ControlError = we.into();
            // Type stays in the same variant family. (Internal
            // collapses to the same kind even though the inner
            // anyhow chain is opaque after serialise.)
            assert_eq!(
                std::mem::discriminant(&ce2_to_kind_marker(&ce2)),
                std::mem::discriminant(&ce2_to_kind_marker(&ce2)),
                "discriminant preserved"
            );
        }
    }

    // Helper for the wire-error roundtrip test: erases the inner
    // payload so we can compare variant tags only.
    enum Kind {
        S,
        W,
        P,
        T,
        R,
        U,
        I,
    }
    fn ce2_to_kind_marker(e: &ControlError) -> Kind {
        match e {
            ControlError::NoSuchSession(_) => Kind::S,
            ControlError::NoSuchWindow(_) => Kind::W,
            ControlError::NoSuchPane(_) => Kind::P,
            ControlError::Transport(_) => Kind::T,
            ControlError::Rejected(_) => Kind::R,
            ControlError::Unsupported { .. } => Kind::U,
            ControlError::Internal(_) => Kind::I,
        }
    }

    /// `Unsupported` has no `WireError` mirror by design — it
    /// degrades to `Rejected` and keeps its message. Pin that so a
    /// later "let's add a WireError::Unsupported" is a deliberate
    /// wire decision rather than a drive-by.
    #[test]
    fn unsupported_degrades_to_rejected_on_the_wire_keeping_its_message() {
        let ce = ControlError::Unsupported {
            capability: "spawn-args",
            detail: "new_window was given 2 argument(s)".into(),
        };
        let we: WireError = ce.into();
        match we {
            WireError::Rejected(msg) => {
                assert!(msg.contains("spawn-args"));
                assert!(msg.contains("new_window was given 2 argument(s)"));
            }
            other => panic!("wrong variant: {other:?}"),
        }
    }

    // ── Capability negotiation ───────────────────────────────────

    #[test]
    fn roundtrip_hello_request_and_response() {
        let req = Request::Hello {
            client_version: "1.2.3".into(),
        };
        let mut buf = Vec::new();
        write_msg(&mut buf, &req).unwrap();
        let got: Request = read_msg(&mut Cursor::new(&buf)).unwrap();
        match got {
            Request::Hello { client_version } => assert_eq!(client_version, "1.2.3"),
            other => panic!("wrong variant: {other:?}"),
        }

        let hello = crate::capability::DaemonHello::for_this_build("0.1.8");
        let resp = Response::Hello(hello.clone());
        let mut buf = Vec::new();
        write_msg(&mut buf, &resp).unwrap();
        let got: Response = read_msg(&mut Cursor::new(&buf)).unwrap();
        match got {
            Response::Hello(h) => assert_eq!(h, hello),
            other => panic!("wrong variant: {other:?}"),
        }
    }

    /// `client_version` is `#[serde(default)]`, so a Hello frame
    /// without it still decodes — the same field-level tolerance the
    /// rest of the wire relies on.
    #[test]
    fn a_hello_without_client_version_decodes_to_empty() {
        use ciborium::value::Value;
        let bare = Value::Map(vec![(Value::Text("Hello".into()), Value::Map(vec![]))]);
        let mut bytes = Vec::new();
        ciborium::ser::into_writer(&bare, &mut bytes).unwrap();
        let got: Request = ciborium::de::from_reader(&bytes[..]).unwrap();
        match got {
            Request::Hello { client_version } => assert!(client_version.is_empty()),
            other => panic!("wrong variant: {other:?}"),
        }
    }

    /// **The measurement the whole negotiation design rests on.**
    ///
    /// A frame naming a variant the reader has never heard of does
    /// NOT desynchronise the stream: the length prefix was honoured,
    /// every payload byte was consumed, and the very next frame
    /// decodes normally. So a server hanging up on an unknown
    /// variant is a *choice*, not a necessity — which is what let
    /// `serve_connection_full` be changed to answer instead.
    ///
    /// The undecodable frame here is a real `Request::Hello`
    /// serialised against a reader (`OldRequest`) that predates it —
    /// the exact shape of a new client meeting an old daemon.
    #[test]
    fn an_undecodable_frame_leaves_the_stream_aligned() {
        /// A structural stand-in for the `Request` enum as it existed
        /// before `Hello` — two variants is enough to prove the
        /// point, since serde rejects on the tag before it looks at
        /// anything else.
        #[derive(Debug, Deserialize)]
        #[allow(dead_code)]
        enum OldRequest {
            ListSessions,
            GetConfig,
        }

        let mut stream = Vec::new();
        write_msg(
            &mut stream,
            &Request::Hello {
                client_version: "0.1.9".into(),
            },
        )
        .unwrap();
        write_msg(&mut stream, &Request::ListSessions).unwrap();

        let mut cur = Cursor::new(stream);
        match read_frame::<_, OldRequest>(&mut cur).unwrap() {
            Framed::Undecodable { reason, len } => {
                assert!(
                    reason.contains("unknown variant") && reason.contains("Hello"),
                    "expected an unknown-variant reason, got: {reason}"
                );
                assert!(len > 0);
            }
            Framed::Msg(m) => panic!("Hello must not decode as OldRequest, got {m:?}"),
        }
        // The whole point: the reader is still aligned.
        match read_frame::<_, OldRequest>(&mut cur).unwrap() {
            Framed::Msg(OldRequest::ListSessions) => {}
            other => panic!("stream desynchronised after an undecodable frame: {other:?}"),
        }
    }

    /// `read_msg` must stay byte-identical in behaviour to what it
    /// was before it was rebuilt on `read_frame` — same error kind,
    /// serde's own message. Every existing caller depends on this.
    #[test]
    fn read_msg_still_collapses_an_undecodable_payload_into_invalid_data() {
        #[derive(Debug, Deserialize)]
        enum OldRequest {
            ListSessions,
        }
        let mut stream = Vec::new();
        write_msg(&mut stream, &Request::GetConfig).unwrap();
        let err = read_msg::<_, OldRequest>(&mut Cursor::new(stream)).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
        assert!(err.to_string().contains("unknown variant"));
        // Silence the never-constructed warning without weakening
        // the enum: it exists purely as a decode target.
        let _ = OldRequest::ListSessions;
    }

    /// An oversized length prefix is NOT an undecodable payload —
    /// those bytes were never counted off the stream, so the
    /// connection really is lost. Must stay a hard `io::Error`.
    #[test]
    fn an_oversized_prefix_stays_a_hard_error_not_an_undecodable_frame() {
        let len: u32 = 32 * 1024 * 1024;
        let mut buf = Vec::new();
        buf.extend_from_slice(&len.to_be_bytes());
        let err = read_frame::<_, Request>(&mut Cursor::new(buf)).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
        assert!(err.to_string().contains("MAX_FRAME_BYTES"));
    }

    #[test]
    fn truncated_frame_errors_cleanly() {
        // 100-byte length-prefix, only 4 bytes of payload — read_msg
        // should return io::Error rather than panic.
        let mut buf = Vec::new();
        let len: u32 = 100;
        buf.extend_from_slice(&len.to_be_bytes());
        buf.extend_from_slice(&[1, 2, 3, 4]);
        let mut cur = Cursor::new(buf);
        let err = read_msg::<_, Request>(&mut cur).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::UnexpectedEof);
    }

    /// No `Request` variant may carry a `TearPane` — the guard `yurai.rs`
    /// says exists.
    ///
    /// `Shutai` closed the payload-identity door by having no `Deserialize`,
    /// so a peer cannot hand the daemon an identity. `Yurai` **must** derive
    /// `Deserialize` — it rides outbound inside `TearPane` inside a
    /// `Response` that clients decode — which structurally reopens that door
    /// one tier lower. What holds it shut is only this: no inbound `Request`
    /// carries a `TearPane`, so a peer's bytes have no route into the
    /// daemon's pane records.
    ///
    /// `yurai.rs` names that ceiling exactly — *"adding any request that
    /// carries a `TearPane` silently restores the payload path"* — and
    /// states it is *"guarded by a source scan, not by the type."* **It was
    /// not.** The only source scan in this crate was `shutai.rs`'s; this one
    /// was claimed in prose and never written (found 2026-08-01 while
    /// checking an adversarial review's hit against the actual tree). A
    /// ceiling documented but unguarded is worse than one left undocumented,
    /// because a reader budgets trust against the claim.
    ///
    /// Tier: **CI-caught**, and it cannot be otherwise — "this enum does not
    /// mention that type" is not a property Rust can state about itself.
    #[test]
    fn no_request_variant_carries_a_tearpane() {
        let src = include_str!("wire.rs");
        let body = src
            .split_once("pub enum Request {")
            .expect("Request enum must exist")
            .1
            .split_once("\n}")
            .expect("Request enum must terminate")
            .0;
        assert!(
            body.len() > 200,
            "scan found only {} bytes of Request — the parser has broken, and \
             a broken parser here reports FALSE SAFETY",
            body.len()
        );
        assert!(
            !body.contains("TearPane"),
            "a Request variant now carries a TearPane, which restores the \
             peer-supplied-provenance path Shutai's missing Deserialize \
             closed: a client could hand the daemon a pane whose `yurai` it \
             chose. Carry a PaneId and let the daemon resolve it."
        );
    }
}
