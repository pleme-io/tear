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
    ControlError, Direction, PaneId, PaneSnapshot, SessionId, TearPane, TearSession, TearWindow,
    WindowId,
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
    SendKeys {
        id: PaneId,
        bytes: Vec<u8>,
    },
    // ── Rendering (Phase 2) ──────────────────────────────────────
    PaneSnapshot(PaneId),
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
}
