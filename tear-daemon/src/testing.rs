//! In-memory test scaffolding for `serve_connection_with_auth` and
//! any future protocol-level test that wants to drive the daemon
//! without binding a real UDS / TCP socket.
//!
//! Gated behind the `testing` feature so the helpers are available
//! to downstream test crates (`tear`'s integration tests) without
//! polluting the production build. The crate's own unit tests
//! enable the gate via the standard `#[cfg(any(test, feature =
//! "testing"))]` pattern in lib.rs.
//!
//! ## Example
//!
//! ```ignore
//! use std::io::Cursor;
//! use std::sync::Arc;
//! use std::sync::mpsc::channel;
//! use tear_core::InProcess;
//! use tear_config::LiveConfig;
//! use tear_daemon::testing::{DuplexStream, drain_responses};
//! use tear_types::wire::{write_msg, Request};
//!
//! // Pre-encode the request frames.
//! let mut input = Vec::new();
//! write_msg(&mut input, &Request::ListSessions).unwrap();
//!
//! let (tx, rx) = channel::<u8>();
//! let stream = DuplexStream::new(input, tx);
//! let inproc = Arc::new(InProcess::new());
//! let live = Arc::new(LiveConfig::default());
//! let _ = tear_daemon::serve_connection_with_auth(
//!     stream, inproc, live, None, None,
//! );
//!
//! let responses = drain_responses(&rx);
//! ```

use std::io::{self, Cursor, Read, Write};
use std::sync::mpsc::{Receiver, Sender};
use std::time::Duration;

use tear_types::wire::{read_msg, Response};

/// In-memory bidirectional pipe for `serve_connection_with_auth`.
/// Reads pre-encoded request frames from `r`; writes response
/// bytes one-at-a-time onto `w` so the test can drain them out of
/// the receiver after the connection closes.
///
/// The one-byte-per-send shape is intentional — it matches what
/// `serde::Serializer` does internally when writing to an
/// unbuffered sink, and ensures the test sees no partial-frame
/// timing artefact.
pub struct DuplexStream {
    r: Cursor<Vec<u8>>,
    w: Sender<u8>,
}

impl DuplexStream {
    #[must_use]
    pub fn new(input: Vec<u8>, sink: Sender<u8>) -> Self {
        Self {
            r: Cursor::new(input),
            w: sink,
        }
    }
}

impl Read for DuplexStream {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.r.read(buf)
    }
}

impl Write for DuplexStream {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        for &b in buf {
            self.w
                .send(b)
                .map_err(|_| io::Error::new(io::ErrorKind::BrokenPipe, "rx dropped"))?;
        }
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// Drain every byte the daemon wrote to the receiver and decode it
/// as a stream of framed [`Response`] messages. Stops when the
/// receiver times out (50 ms — enough for synchronous in-process
/// drives).
#[must_use]
pub fn drain_responses(rx: &Receiver<u8>) -> Vec<Response> {
    let mut bytes = Vec::new();
    while let Ok(b) = rx.recv_timeout(Duration::from_millis(50)) {
        bytes.push(b);
    }
    let mut cur = Cursor::new(bytes);
    let mut out = Vec::new();
    while let Ok(r) = read_msg::<_, Response>(&mut cur) {
        out.push(r);
    }
    out
}
