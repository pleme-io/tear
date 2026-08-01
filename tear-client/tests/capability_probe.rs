//! Capability negotiation, driven against BOTH ends for real.
//!
//! The interesting half is not "a current client talks to a current
//! daemon" — it is what happens when a client that knows
//! `Request::Hello` meets a daemon that does not. That daemon is not
//! hypothetical: the one running on the operator's machine while this
//! was written is `tear-0.1.8` out of the nix store, launchd-managed,
//! so restarting it re-launches the same old build. Every client
//! built after this lands will meet it first.
//!
//! So the pre-capability daemon is reconstructed here rather than
//! mocked away: [`old_serve_loop`] is the serve loop as it was —
//! same 4-byte-big-endian CBOR framing, same one-Response-per-Request
//! contract, a `Request` enum with no `Hello` variant, and critically
//! the same `Err(e) => return Err(e)` on a read failure, which is
//! what closes the socket on an unknown variant.

use std::io::{self, Read, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;

use serde::Deserialize;

use tear_client::Client;
use tear_types::wire::{read_msg, write_msg, Response};
use tear_types::{Capability, ControlError, MultiplexerControl, SessionId, WindowId};

// ── The pre-capability daemon, reconstructed ─────────────────────

/// `tear-daemon`'s `Request` enum as it stood **before** capability
/// negotiation — and, for `NewWindow`, before `args` existed at all
/// (commit `5974375`).
///
/// Both omissions are deliberate and each proves a different thing:
///
/// - no `Hello` variant → a new client's probe frame is undecodable,
///   which is what makes the old loop hang up;
/// - no `args` field on `NewWindow` → a new client's frame decodes
///   *cleanly* and the arguments vanish, with no error anywhere.
///   That silent drop is the bug the whole capability set exists to
///   convert into a refusal.
#[derive(Debug, Deserialize)]
enum OldRequest {
    ListSessions,
    NewWindow {
        #[allow(dead_code)]
        session: SessionId,
        #[allow(dead_code)]
        name: String,
        #[allow(dead_code)]
        shell: String,
    },
}

/// The serve loop as it was. The one line that matters is the
/// `Err(e) => return Err(e)` arm: a frame the enum cannot decode is
/// indistinguishable from a dead socket, so the connection ends.
fn old_serve_loop<S: Read + Write>(mut stream: S, seen: &Arc<Mutex<Vec<&'static str>>>) {
    loop {
        let req: OldRequest = match read_msg(&mut stream) {
            Ok(r) => r,
            Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => return,
            // ← THE OLD BEHAVIOUR. An unknown variant lands here and
            //   takes the whole connection down with it.
            Err(_) => return,
        };
        let resp = match req {
            OldRequest::ListSessions => {
                seen.lock().unwrap().push("ListSessions");
                Response::Sessions(vec![])
            }
            OldRequest::NewWindow { .. } => {
                seen.lock().unwrap().push("NewWindow");
                Response::WindowId(WindowId::from_seed("old-window"))
            }
        };
        if write_msg(&mut stream, &resp).is_err() {
            return;
        }
    }
}

/// A private UDS with a pre-capability daemon behind it. Returns the
/// path plus the log of requests that daemon actually decoded.
fn spawn_pre_capability_daemon(label: &str) -> (PathBuf, Arc<Mutex<Vec<&'static str>>>) {
    static SEQ: AtomicU32 = AtomicU32::new(0);
    let mut socket = std::env::temp_dir();
    socket.push(format!(
        "tear-precap-{label}-{}-{}.sock",
        std::process::id(),
        SEQ.fetch_add(1, Ordering::Relaxed)
    ));
    let _ = std::fs::remove_file(&socket);
    let listener = UnixListener::bind(&socket).expect("bind pre-capability socket");
    let seen = Arc::new(Mutex::new(Vec::new()));
    let seen_for_accept = seen.clone();
    thread::Builder::new()
        .name("precap-accept".into())
        .spawn(move || {
            for stream in listener.incoming() {
                let Ok(stream) = stream else { return };
                let seen_for_conn = seen_for_accept.clone();
                let _ = thread::Builder::new()
                    .name("precap-conn".into())
                    .spawn(move || old_serve_loop(stream, &seen_for_conn));
            }
        })
        .expect("spawn accept thread");
    // Let the accept loop reach `incoming()` before the first dial.
    thread::sleep(std::time::Duration::from_millis(50));
    (socket, seen)
}

/// Sanity-check the reconstruction itself before trusting anything
/// built on it: the old loop really does hang up on a frame it
/// cannot decode, and the client really does observe that as a lost
/// connection rather than an error reply.
///
/// Without this, a green degradation test could just as well mean
/// "the reconstruction accidentally answers everything".
#[test]
fn the_reconstructed_old_loop_really_does_hang_up_on_an_unknown_variant() {
    let (socket, _seen) = spawn_pre_capability_daemon("hangup");
    let mut raw = UnixStream::connect(&socket).expect("dial");

    // Positive control: a request it DOES know is answered.
    write_msg(&mut raw, &tear_types::wire::Request::ListSessions).unwrap();
    let resp: Response = read_msg(&mut raw).expect("known request must be answered");
    assert!(matches!(resp, Response::Sessions(_)), "got {resp:?}");

    // Now the probe. No reply, and the socket closes.
    write_msg(
        &mut raw,
        &tear_types::wire::Request::Hello {
            client_version: "0.1.9".into(),
        },
    )
    .unwrap();
    let err = read_msg::<_, Response>(&mut raw)
        .expect_err("a pre-capability daemon must NOT answer Hello");
    assert_eq!(
        err.kind(),
        io::ErrorKind::UnexpectedEof,
        "expected the connection to be closed, got: {err}"
    );

    let _ = std::fs::remove_file(&socket);
}

/// **The fail-once seal.** A new client against a pre-capability
/// daemon must come back with "protocol 0 / no capabilities" and a
/// **working connection** — not a dropped one.
///
/// The probe costs that first connection; the client re-dials and the
/// caller never sees the loss. Remove the re-dial in
/// `Client::probe_capabilities` and `list_sessions()` below goes red
/// with a broken pipe.
#[test]
fn a_new_client_against_a_pre_capability_daemon_degrades_to_no_capabilities() {
    let (socket, seen) = spawn_pre_capability_daemon("degrade");

    let client = Client::connect(&socket)
        .expect("connecting to a pre-capability daemon must SUCCEED, not error");

    // The verdict.
    assert!(
        client.daemon().is_pre_capability(),
        "expected protocol 0, got {:?}",
        client.daemon()
    );
    assert_eq!(
        client.daemon().version(),
        None,
        "a daemon that never answered must not be assigned a version"
    );
    assert!(client.daemon().capability_names().is_empty());
    assert!(!client.daemon().has(Capability::SpawnArgs));

    // The connection is USABLE. This is the assertion that separates
    // "degraded" from "dead", and the one the naive Hello frame would
    // have failed.
    let sessions = client
        .list_sessions()
        .expect("the client must still work after the probe was refused");
    assert!(sessions.is_empty());
    assert_eq!(
        seen.lock().unwrap().as_slice(),
        &["ListSessions"],
        "the re-dialled connection must be the one carrying real traffic"
    );

    let _ = std::fs::remove_file(&socket);
}

/// The refusal is **scoped to the call that needs the capability**.
///
/// A caller passing no args is unaffected — its request goes out and
/// is served. A caller passing args gets a typed `Unsupported` naming
/// the field, and the frame is never written, so the daemon has no
/// chance to accept-and-drop it.
#[test]
fn spawn_args_is_refused_at_the_call_site_and_only_there() {
    let (socket, seen) = spawn_pre_capability_daemon("callsite");
    let client = Client::connect(&socket).expect("connect");
    let session = SessionId::from_seed("s");

    // (a) No args → unaffected. Reaches the old daemon and succeeds.
    client
        .new_window(session, "plain", "/bin/sh", &[])
        .expect("a caller that needs no args must not be refused");

    // (b) Args → typed refusal, naming the field.
    let err = client
        .new_window(session, "with-args", "/bin/sh", &["-l".into()])
        .expect_err("args against a daemon that cannot read them must be refused");
    match err {
        ControlError::Unsupported { capability, detail } => {
            assert_eq!(capability, "spawn-args");
            assert!(detail.contains("new_window was given 1 argument(s)"), "{detail}");
            assert!(detail.contains("predates capability negotiation"), "{detail}");
        }
        other => panic!("expected ControlError::Unsupported, got: {other:?}"),
    }

    // (c) The refusal happened BEFORE the wire. The old daemon saw
    //     exactly one NewWindow — the args-free one. Had the frame
    //     gone out, it would have decoded cleanly (no
    //     `deny_unknown_fields`) and the arguments would have
    //     vanished with no error: the original silent degradation.
    assert_eq!(
        seen.lock().unwrap().as_slice(),
        &["NewWindow"],
        "the args-bearing request must never reach the wire"
    );

    // Same rule on the other two arg-bearing call sites.
    for err in [
        client
            .split_pane(
                tear_types::PaneId::from_seed("p"),
                tear_types::Direction::Right,
                "/bin/sh",
                &["-l".into()],
            )
            .unwrap_err(),
        client
            .new_session_with_source_and_size(
                "s",
                "/bin/sh",
                &["-l".into()],
                tear_types::SessionSource::Human,
                (80, 24),
            )
            .unwrap_err(),
    ] {
        assert!(
            matches!(err, ControlError::Unsupported { capability: "spawn-args", .. }),
            "got: {err:?}"
        );
    }
    assert_eq!(
        seen.lock().unwrap().len(),
        1,
        "no further frames may have reached the daemon"
    );

    let _ = std::fs::remove_file(&socket);
}

// ── Positive control: a current daemon ───────────────────────────

/// The other half of the calibration. Against a daemon built from
/// this tree the probe succeeds, the version is the daemon's own, and
/// `spawn-args` goes through — so a red degradation test above means
/// the degradation path broke, not that the probe never works.
#[test]
fn a_current_daemon_answers_the_probe_and_accepts_args() {
    let harness = tear_daemon::testing::DaemonHarness::new("cap-probe");
    let client = Client::connect(harness.socket()).expect("connect");

    assert!(
        !client.daemon().is_pre_capability(),
        "a current daemon must answer the probe"
    );
    assert_eq!(
        client.daemon().version(),
        Some(env!("CARGO_PKG_VERSION")),
        "the daemon reports its own version"
    );
    assert!(client.daemon().has(Capability::SpawnArgs));
    assert!(client.daemon().has(Capability::PaneYurai));
    assert!(client.daemon().has(Capability::Freio));
    // The exact list a LIVE daemon advertises. Kept exact rather than
    // relaxed to `has`: this assert is what makes adding a capability a
    // conscious act, and it caught the freio/yurai addition at the
    // integration level rather than in a unit test.
    //
    // SORTED, not in `Capability::ALL` order — `capability_names()`
    // returns a sorted view, which is the right contract for a set and is
    // what this assert pins.
    assert_eq!(
        client.daemon().capability_names(),
        vec!["freio", "pane-yurai", "spawn-args"]
    );

    // And the trait-level view agrees, so a consumer holding a
    // `&dyn MultiplexerControl` gates the same way.
    let as_trait: &dyn MultiplexerControl = &client;
    assert!(as_trait.capabilities().has(Capability::SpawnArgs));

    // The call that would have been refused now goes through.
    let session = client
        .new_session_with_source_and_size(
            "cap",
            "/bin/sh",
            &["-c".into(), "sleep 30".into()],
            tear_types::SessionSource::Human,
            (80, 24),
        )
        .expect("args must be accepted by a daemon that advertises spawn-args");
    let live = client.get_session(session).expect("get_session");
    let seeded = live.panes.values().next().expect("session has a seed pane");
    assert_eq!(
        seeded.args,
        vec!["-c".to_owned(), "sleep 30".to_owned()],
        "the daemon must have recorded the args it advertised support for"
    );

    let _ = client.kill_session(session);
}

/// An in-process backend needs no probe: it *is* this build, so the
/// trait default answers with the full local capability set. Pins
/// that the default is not accidentally empty (which would refuse
/// every embedded caller) nor accidentally overridden.
#[test]
fn the_in_process_backend_has_this_builds_capabilities_without_a_probe() {
    let inproc = tear_core::InProcess::new();
    let caps = inproc.capabilities();
    assert!(!caps.is_pre_capability());
    assert!(caps.has(Capability::SpawnArgs));
    assert_eq!(caps.version(), Some(env!("CARGO_PKG_VERSION")));
}
