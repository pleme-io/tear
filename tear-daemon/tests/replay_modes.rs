#![cfg(all(feature = "testing", feature = "bench-probes"))]

use std::os::unix::net::UnixStream;
use std::path::Path;
use std::thread;
use std::time::{Duration, Instant};

use tear_core::inproc::InProcess;
use tear_core::probes::consume_replays;
use tear_daemon::testing::DaemonHarness;
use tear_types::modes::MouseTracking;
use tear_types::wire::{Request, Response, read_msg, write_msg};
use tear_types::{MultiplexerControl, PaneId, SessionSource};

const VIM: &str = "stty raw -echo; printf '\\033[?1049h\\033[?1h\\033=\\033[?2004h\\033[?1000h\\033[?1006h\\033[?25l\\033[2 q\\033[>1u\\033[>4;2m'; printf READY; exec cat";
const SHELL: &str = "stty raw -echo; i=0; while [ $i -lt 200 ]; do printf 'history %d\\r\\n' $i; i=$((i+1)); done; printf '\\033[6n\\033[?2004h\\033[>3u'; printf READY; exec cat";

fn pane(inproc: &InProcess, script: &str) -> PaneId {
    let sid = inproc
        .new_session_with_source_and_size(
            "replay",
            "/bin/sh",
            &["-c".to_string(), script.to_string()],
            SessionSource::Human,
            (80, 24),
        )
        .expect("new session");
    let pane = inproc.with_registry(|r| {
        r.sessions
            .get(&sid)
            .and_then(|s| s.panes.keys().next().copied())
            .expect("a pane")
    });
    let deadline = Instant::now() + Duration::from_secs(10);
    while !inproc
        .pane_snapshot(pane)
        .unwrap()
        .to_text()
        .contains("READY")
    {
        assert!(Instant::now() < deadline, "the pane never printed READY");
        thread::sleep(Duration::from_millis(5));
    }
    pane
}

fn first_frame(socket: &Path, pane: PaneId) -> Vec<u8> {
    let mut s = UnixStream::connect(socket).expect("connect");
    s.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
    write_msg(&mut s, &Request::Subscribe(pane)).unwrap();
    assert!(matches!(
        read_msg::<_, Response>(&mut s).unwrap(),
        Response::Ok
    ));
    match read_msg::<_, Response>(&mut s).unwrap() {
        Response::PaneBytes(b) => b,
        other => panic!("the first frame after Ok is the replay, got {other:?}"),
    }
}

#[test]
fn the_daemon_advertises_replay_modes() {
    let h = DaemonHarness::new("replay-hello");
    let mut s = UnixStream::connect(h.socket()).unwrap();
    write_msg(
        &mut s,
        &Request::Hello {
            client_version: "test".into(),
        },
    )
    .unwrap();
    match read_msg::<_, Response>(&mut s).unwrap() {
        Response::Hello(hello) => assert!(
            hello.capabilities.iter().any(|c| c == "replay-modes"),
            "{:?}",
            hello.capabilities
        ),
        other => panic!("{other:?}"),
    }
}

#[test]
fn an_attach_into_vim_restores_its_modes_from_the_daemon_s_one_replay() {
    let h = DaemonHarness::new("replay-vim");
    let inproc = h.daemon().inproc();
    let pane = pane(inproc, VIM);
    let frame = first_frame(h.socket(), pane);
    let authority = inproc.pane_snapshot(pane).unwrap();
    let once = consume_replays(authority.cols, authority.rows, &[&frame]);
    assert!(once.modes.cursor_keys.enabled(), "DECCKM");
    assert!(once.modes.bracketed_paste.enabled(), "2004");
    assert_eq!(once.modes.mouse, MouseTracking::Click, "1000");
    assert!(once.modes.mouse_sgr().enabled(), "1006");
    assert_eq!(once.modes, authority.modes);
    let twice = consume_replays(authority.cols, authority.rows, &[&frame, &frame]);
    assert_eq!(twice.modes, once.modes);
}

#[test]
fn an_attach_leaves_exactly_the_pane_s_history_and_writes_no_answer() {
    let h = DaemonHarness::new("replay-shell");
    let inproc = h.daemon().inproc();
    let pane = pane(inproc, SHELL);
    let frame = first_frame(h.socket(), pane);
    let authority = inproc.pane_snapshot(pane).unwrap();
    assert!(authority.scrollback.len() > 150);
    let got = consume_replays(authority.cols, authority.rows, &[&frame]);
    assert_eq!(got.scrollback_rows, authority.scrollback.len());
    assert_eq!(got.answers, 0, "a replay asks the consumer nothing");
    assert_eq!(got.modes, authority.modes);
}

const OVER_THE_CAP: &str = "stty raw -echo; r=''; j=0; while [ $j -lt 40 ]; do r=\"$r\\033[31mX\\033[32mY\"; j=$((j+1)); done; i=0; while [ $i -lt 14000 ]; do printf \"$r\\r\\n\"; i=$((i+1)); done; printf '\\033[0m\\033[?1h\\033[?2004hREADY'; exec cat";

#[test]
fn a_replay_over_the_frame_cap_leaves_out_the_oldest_history_and_the_stream_stays_live() {
    let h = DaemonHarness::new("replay-over-cap");
    let inproc = h.daemon().inproc();
    let sid = inproc
        .new_session_with_source_and_size(
            "over-cap",
            "/bin/sh",
            &["-c".to_string(), OVER_THE_CAP.to_string()],
            SessionSource::Human,
            (80, 24),
        )
        .expect("new session");
    let pane = inproc.with_registry(|r| {
        r.sessions
            .get(&sid)
            .and_then(|s| s.panes.keys().next().copied())
            .expect("a pane")
    });
    let deadline = Instant::now() + Duration::from_secs(120);
    while !inproc
        .pane_snapshot(pane)
        .unwrap()
        .to_text()
        .contains("READY")
    {
        assert!(Instant::now() < deadline, "the pane never printed READY");
        thread::sleep(Duration::from_millis(50));
    }
    let authority = inproc.pane_snapshot(pane).unwrap();
    assert!(
        authority.to_ansi().len() > tear_types::wire::MAX_FRAME_BYTES,
        "the pane's whole replay is over the cap"
    );
    let mut s = UnixStream::connect(h.socket()).expect("connect");
    s.set_read_timeout(Some(Duration::from_secs(30))).unwrap();
    write_msg(&mut s, &Request::Subscribe(pane)).unwrap();
    assert!(matches!(
        read_msg::<_, Response>(&mut s).unwrap(),
        Response::Ok
    ));
    let frame = match read_msg::<_, Response>(&mut s).unwrap() {
        Response::PaneBytes(b) => b,
        other => panic!("the first frame after Ok is the replay, got {other:?}"),
    };
    assert!(frame.len() <= tear_types::wire::MAX_PANE_BYTES);
    let got = consume_replays(authority.cols, authority.rows, &[&frame]);
    assert!(got.scrollback_rows > 0, "the newest history is kept");
    assert!(got.scrollback_rows < authority.scrollback.len());
    assert_eq!(got.modes, authority.modes);
    inproc.send_keys(pane, b"LIVE").expect("type");
    let mut seen = Vec::new();
    while !seen.windows(4).any(|w| w == b"LIVE") {
        match read_msg::<_, Response>(&mut s).expect("the stream stays open") {
            Response::PaneBytes(b) => seen.extend_from_slice(&b),
            other => panic!("the stream after the replay carries the pane's bytes, got {other:?}"),
        }
    }
    inproc.kill_session(sid).unwrap();
}
