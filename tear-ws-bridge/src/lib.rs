//! Tear WebSocket bridge — turns the typed CBOR wire that
//! `tear-daemon` speaks (over UDS or TCP) into a `ws://` endpoint
//! that browsers / wasm renderers can attach to without speaking
//! UDS / TCP directly.
//!
//! ## Wire
//!
//! Each WebSocket binary message is a single length-prefixed CBOR
//! frame — the same shape `tear-types::wire::write_msg` produces
//! and `read_msg` consumes. A browser-side client just needs to
//! length-prefix its CBOR Requests in WS binary frames and decode
//! Responses the same way. No protocol translation.
//!
//! ## Architecture
//!
//! For each incoming WS connection the bridge opens a fresh
//! connection to the local tear-daemon (via [`tear_client::Transport`])
//! and pipes bytes both directions. Reader threads on each side
//! shuttle frames; the bridge itself is mostly a pair of byte
//! pumps with one small framing concern: the WS layer is
//! message-framed (one binary message = one frame), while the
//! UDS/TCP wire is stream-of-bytes with embedded length prefixes.
//! The bridge re-frames in both directions.
//!
//! ## Why a separate crate
//!
//! `tear-daemon` deliberately doesn't pull in `tungstenite` —
//! daemon footprint stays tight; WS is an opt-in surface
//! operators activate when they want browser/wasm access. The
//! bridge runs as a sidecar daemon, gating the WS port through
//! its own systemd unit / launchd plist so it can be enabled or
//! disabled independently.

#![forbid(unsafe_code)]

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::Duration;

use anyhow::Result;
use tear_client::Transport;
use tracing::{debug, error, info, warn};
use tungstenite::Message;

/// Configuration for the bridge run loop.
#[derive(Clone, Debug)]
pub struct BridgeConfig {
    /// WebSocket listen address (e.g. `127.0.0.1:8181` or
    /// `0.0.0.0:8181`).
    pub listen: SocketAddr,
    /// Where to dial the tear-daemon. UDS path or `tcp://host:port`.
    pub tear: Transport,
}

/// Handle to a running bridge — holds the stop flag and the
/// accept-loop join handle. Drop joins.
pub struct BridgeHandle {
    stop: Arc<AtomicBool>,
    accept: Option<thread::JoinHandle<()>>,
    pub listen: SocketAddr,
}

impl BridgeHandle {
    pub fn stop(mut self) {
        self.signal_and_join();
    }

    fn signal_and_join(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        // Tickle the accept loop — TCP set_nonblocking + sleep
        // means the loop polls the stop flag every 50ms.
        if let Some(j) = self.accept.take() {
            let _ = j.join();
        }
    }
}

impl Drop for BridgeHandle {
    fn drop(&mut self) {
        if self.accept.is_some() {
            self.signal_and_join();
        }
    }
}

/// Spawn the bridge. Returns when the listener is bound; the
/// accept loop runs in a background thread until [`BridgeHandle`]
/// is stopped.
pub fn start(cfg: BridgeConfig) -> Result<BridgeHandle> {
    let listener = TcpListener::bind(cfg.listen)?;
    let bound = listener.local_addr()?;
    listener.set_nonblocking(true)?;
    info!(
        listen = %bound,
        tear = %cfg.tear.display_string(),
        "tear-ws-bridge listening"
    );

    let stop = Arc::new(AtomicBool::new(false));
    let stop_for_accept = stop.clone();
    let tear_for_accept = cfg.tear.clone();

    let accept = thread::Builder::new()
        .name("tear-ws-bridge-accept".into())
        .spawn(move || accept_loop(listener, stop_for_accept, tear_for_accept))?;

    Ok(BridgeHandle {
        stop,
        accept: Some(accept),
        listen: bound,
    })
}

fn accept_loop(listener: TcpListener, stop: Arc<AtomicBool>, tear: Transport) {
    loop {
        if stop.load(Ordering::SeqCst) {
            debug!("ws-bridge accept loop: stop requested");
            return;
        }
        match listener.accept() {
            Ok((stream, peer)) => {
                debug!(peer = %peer, "ws connection accepted");
                let tear_for_conn = tear.clone();
                let _ = thread::Builder::new()
                    .name("tear-ws-bridge-conn".into())
                    .spawn(move || {
                        let _ = stream.set_nonblocking(false);
                        if let Err(e) = serve_ws_connection(stream, tear_for_conn) {
                            warn!(error = %e, "ws connection ended");
                        }
                    });
            }
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                std::thread::sleep(Duration::from_millis(50));
            }
            Err(e) => {
                error!(error = %e, "ws accept failed");
                std::thread::sleep(Duration::from_millis(50));
            }
        }
    }
}

/// Handle one WS connection: upgrade, open a tear connection,
/// then run two byte pumps until either side disconnects.
fn serve_ws_connection(tcp: TcpStream, tear: Transport) -> Result<()> {
    let ws = tungstenite::accept(tcp)?;
    // Dial the upstream tear daemon. The bridge gets its own
    // connection per WS client so sessions are isolated.
    let backend = match &tear {
        Transport::Unix(p) => TearStream::Unix(std::os::unix::net::UnixStream::connect(p)?),
        Transport::Tcp(addr) => TearStream::Tcp(TcpStream::connect(addr)?),
    };
    pump(ws, backend)
}

/// Wraps either UDS or TCP into one type with Read+Write+Clone.
enum TearStream {
    Unix(std::os::unix::net::UnixStream),
    Tcp(TcpStream),
}

impl TearStream {
    fn try_clone(&self) -> std::io::Result<Self> {
        match self {
            TearStream::Unix(s) => s.try_clone().map(TearStream::Unix),
            TearStream::Tcp(s) => s.try_clone().map(TearStream::Tcp),
        }
    }
}

impl Read for TearStream {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        match self {
            TearStream::Unix(s) => s.read(buf),
            TearStream::Tcp(s) => s.read(buf),
        }
    }
}

impl Write for TearStream {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        match self {
            TearStream::Unix(s) => s.write(buf),
            TearStream::Tcp(s) => s.write(buf),
        }
    }
    fn flush(&mut self) -> std::io::Result<()> {
        match self {
            TearStream::Unix(s) => s.flush(),
            TearStream::Tcp(s) => s.flush(),
        }
    }
}

/// Bidirectional pump. Spawns a thread that reads tear-daemon's
/// length-prefixed CBOR frames + forwards each as a WS binary
/// message; the main thread reads WS binary messages + writes
/// them verbatim (already length-prefixed) to the daemon.
fn pump(
    mut ws: tungstenite::WebSocket<TcpStream>,
    mut backend: TearStream,
) -> Result<()> {
    // ── daemon → ws thread ───────────────────────────────────
    let backend_for_reader = backend
        .try_clone()
        .map_err(|e| anyhow::anyhow!("clone backend: {e}"))?;
    let ws_writer = ws.get_mut().try_clone()?;
    let _reader = thread::Builder::new()
        .name("tear-ws-bridge-daemon-reader".into())
        .spawn(move || {
            let mut br = std::io::BufReader::new(backend_for_reader);
            let mut writer = tungstenite::WebSocket::from_raw_socket(
                ws_writer,
                tungstenite::protocol::Role::Server,
                None,
            );
            loop {
                let mut len_bytes = [0u8; 4];
                if br.read_exact(&mut len_bytes).is_err() {
                    let _ = writer.close(None);
                    return;
                }
                let len = u32::from_be_bytes(len_bytes) as usize;
                if len > tear_types::wire::MAX_FRAME_BYTES {
                    warn!(len, "ws-bridge daemon→ws: frame too large, closing");
                    let _ = writer.close(None);
                    return;
                }
                let mut payload = vec![0u8; len];
                if br.read_exact(&mut payload).is_err() {
                    let _ = writer.close(None);
                    return;
                }
                // Re-frame: WS binary message contains the
                // length prefix + payload (= one whole CBOR
                // frame as the browser side reads it).
                let mut framed = Vec::with_capacity(4 + len);
                framed.extend_from_slice(&len_bytes);
                framed.extend_from_slice(&payload);
                if writer.send(Message::Binary(framed.into())).is_err() {
                    return;
                }
            }
        })?;

    // ── ws → daemon (main thread loop) ───────────────────────
    loop {
        match ws.read() {
            Ok(Message::Binary(bytes)) => {
                // Bytes are already a complete CBOR frame
                // (length prefix + payload). Forward verbatim
                // — the daemon's read_msg pulls the same shape
                // off any byte stream.
                if backend.write_all(&bytes).is_err() {
                    return Ok(());
                }
                if backend.flush().is_err() {
                    return Ok(());
                }
            }
            Ok(Message::Close(_)) => return Ok(()),
            Ok(_) => continue,
            Err(_) => return Ok(()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn end_to_end_ws_bridge_pipes_request_response() {
        use std::net::TcpStream as StdTcpStream;
        use tear_types::wire::{read_msg, write_msg, Request, Response};

        // Spin up a real tear-daemon on UDS in this process.
        let sock = {
            let mut p = std::env::temp_dir();
            let pid = std::process::id();
            p.push(format!("tear-ws-bridge-test-{pid}.sock"));
            p
        };
        let _ = std::fs::remove_file(&sock);
        let inproc = std::sync::Arc::new(tear_core::InProcess::new());
        let daemon = tear_daemon::start(sock.clone(), inproc).expect("daemon");
        std::thread::sleep(Duration::from_millis(50));

        // Spin up the bridge.
        let bridge = start(BridgeConfig {
            listen: "127.0.0.1:0".parse().unwrap(),
            tear: Transport::Unix(sock.clone()),
        })
        .expect("bridge");
        std::thread::sleep(Duration::from_millis(50));

        // Connect a tungstenite client to the bridge.
        let url = format!("ws://{}/", bridge.listen);
        let stream = StdTcpStream::connect(bridge.listen).expect("connect");
        let (mut ws, _resp) = tungstenite::client(url, stream).expect("ws client");

        // Send a list_sessions request as length-prefixed CBOR
        // inside a WS binary message.
        let mut req_bytes = Vec::new();
        write_msg(&mut req_bytes, &Request::ListSessions).unwrap();
        ws.send(Message::Binary(req_bytes.into())).unwrap();

        // Read the response WS binary message + decode.
        let msg = ws.read().expect("read");
        match msg {
            Message::Binary(b) => {
                let mut cursor = std::io::Cursor::new(b.as_ref());
                let resp: Response = read_msg(&mut cursor).expect("decode");
                match resp {
                    Response::Sessions(s) => {
                        assert_eq!(s.len(), 0, "no sessions on a fresh daemon");
                    }
                    other => panic!("unexpected response: {other:?}"),
                }
            }
            other => panic!("expected binary frame, got {other:?}"),
        }

        ws.close(None).ok();
        bridge.stop();
        daemon.stop();
    }
}
