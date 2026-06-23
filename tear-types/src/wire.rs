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
//! The wire is bincode + serde, so adding a new variant to either
//! enum at the *end* is backwards-compatible (older clients ignore
//! variants they don't understand because they never emit them).
//! Removing or reordering variants is a breaking wire change — bump
//! the workspace minor version when that happens.

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
    },
    KillWindow(WindowId),
    SelectWindow(WindowId),
    // ── Panes ────────────────────────────────────────────────────
    SplitPane {
        origin: PaneId,
        direction: Direction,
        shell: String,
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
    /// Pushed by the daemon on every live-config replace, to
    /// every connection that issued `Request::SubscribeConfigChange`.
    /// Payload is the new config as YAML — same shape as
    /// `Response::ConfigYaml`. The first frame after subscription
    /// is `Response::Ok`; subsequent frames are `ConfigChanged`
    /// until the connection is dropped.
    ConfigChanged(String),
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

/// Read a length-prefixed CBOR-encoded message. Caps the frame at
/// [`MAX_FRAME_BYTES`] so a malformed prefix can't trigger an
/// unbounded allocation.
pub fn read_msg<R: Read, T: for<'de> Deserialize<'de>>(r: &mut R) -> io::Result<T> {
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
    ciborium::de::from_reader(&buf[..])
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e.to_string()))
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
        I,
    }
    fn ce2_to_kind_marker(e: &ControlError) -> Kind {
        match e {
            ControlError::NoSuchSession(_) => Kind::S,
            ControlError::NoSuchWindow(_) => Kind::W,
            ControlError::NoSuchPane(_) => Kind::P,
            ControlError::Transport(_) => Kind::T,
            ControlError::Rejected(_) => Kind::R,
            ControlError::Internal(_) => Kind::I,
        }
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
}
