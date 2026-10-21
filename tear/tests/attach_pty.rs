use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use portable_pty::{CommandBuilder, PtySize, native_pty_system};
use tear_client::Client;
use tear_daemon::testing::DaemonHarness;
use tear_types::{MultiplexerControl, PaneId, PaneState, SessionSource};

struct Term {
    master: Box<dyn portable_pty::MasterPty + Send>,
    writer: Box<dyn Write + Send>,
    child: Box<dyn portable_pty::Child + Send + Sync>,
    seen: Arc<Mutex<Vec<u8>>>,
}

fn wait_for(what: &str, mut f: impl FnMut() -> bool) {
    let end = Instant::now() + Duration::from_secs(20);
    while Instant::now() < end {
        if f() {
            return;
        }
        std::thread::sleep(Duration::from_millis(30));
    }
    panic!("timed out waiting for {what}");
}

impl Term {
    fn spawn(args: &[&str], socket: &Path, home: &Path, size: (u16, u16)) -> Self {
        let pair = native_pty_system()
            .openpty(PtySize {
                rows: size.1,
                cols: size.0,
                pixel_width: 0,
                pixel_height: 0,
            })
            .unwrap();
        let mut cmd = CommandBuilder::new(env!("CARGO_BIN_EXE_tear"));
        cmd.arg("attach");
        for a in args {
            cmd.arg(a);
        }
        cmd.arg("--socket");
        cmd.arg(socket);
        cmd.env("HOME", home);
        cmd.env("XDG_CONFIG_HOME", home.join("config"));
        cmd.env("XDG_RUNTIME_DIR", home.join("run"));
        cmd.env("TERM", "xterm-256color");
        let child = pair.slave.spawn_command(cmd).unwrap();
        drop(pair.slave);
        let mut reader = pair.master.try_clone_reader().unwrap();
        let writer = pair.master.take_writer().unwrap();
        let seen = Arc::new(Mutex::new(Vec::new()));
        let s = seen.clone();
        std::thread::spawn(move || {
            let mut buf = [0u8; 4096];
            while let Ok(n) = reader.read(&mut buf) {
                if n == 0 {
                    break;
                }
                s.lock().unwrap().extend_from_slice(&buf[..n]);
            }
        });
        Self {
            master: pair.master,
            writer,
            child,
            seen,
        }
    }

    fn text(&self) -> String {
        String::from_utf8_lossy(&self.seen.lock().unwrap()).into_owned()
    }

    fn type_bytes(&mut self, b: &[u8]) {
        self.writer.write_all(b).unwrap();
        self.writer.flush().unwrap();
    }

    fn exit_code(&mut self) -> u32 {
        let end = Instant::now() + Duration::from_secs(20);
        loop {
            if let Some(st) = self.child.try_wait().unwrap() {
                return st.exit_code();
            }
            assert!(Instant::now() < end, "tear attach did not exit");
            std::thread::sleep(Duration::from_millis(30));
        }
    }
}

struct Env {
    h: DaemonHarness,
    home: tempfile::TempDir,
}

fn env(label: &str) -> Env {
    let home = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(home.path().join("config/tear")).unwrap();
    std::fs::create_dir_all(home.path().join("run")).unwrap();
    Env {
        h: DaemonHarness::new(label),
        home,
    }
}

fn make(h: &DaemonHarness, name: &str) -> PaneId {
    let c = Client::connect(h.socket()).unwrap();
    let id = c
        .new_session_with_source_and_size(name, "/bin/sh", &[], SessionSource::Human, (80, 24))
        .unwrap();
    c.get_session(id)
        .unwrap()
        .windows
        .values()
        .next()
        .unwrap()
        .active_pane
}

fn write_config(e: &Env, yaml: &str) -> PathBuf {
    let p = e.home.path().join("config/tear/tear.yaml");
    std::fs::write(&p, yaml).unwrap();
    p
}

#[test]
fn attach_type_detach_reattach_through_a_real_pty() {
    let e = env("pty-e2e");
    let pane = make(&e.h, "work");
    let mut t = Term::spawn(&["work"], e.h.socket(), e.home.path(), (100, 30));
    wait_for("shell output", || !t.text().is_empty());
    t.type_bytes(b"echo MARK$((20+22))\r");
    wait_for("command output", || t.text().contains("MARK42"));
    t.type_bytes(&[0x02, b'd']);
    assert_eq!(t.exit_code(), 0);
    assert!(t.text().contains("detached"));

    let c = Client::connect(e.h.socket()).unwrap();
    assert!(matches!(
        c.get_pane(pane).unwrap().state,
        PaneState::Running
    ));

    let mut t2 = Term::spawn(&["work"], e.h.socket(), e.home.path(), (100, 30));
    wait_for("history replay", || t2.text().contains("MARK42"));
    t2.type_bytes(b"echo AGAIN$((1+1))\r");
    wait_for("second command", || t2.text().contains("AGAIN2"));
    t2.type_bytes(&[0x02, b'd']);
    assert_eq!(t2.exit_code(), 0);
}

#[test]
fn the_pty_resize_reaches_the_pane_through_sigwinch() {
    let e = env("pty-winch");
    let pane = make(&e.h, "work");
    let mut t = Term::spawn(&["work"], e.h.socket(), e.home.path(), (60, 20));
    let c = Client::connect(e.h.socket()).unwrap();
    wait_for("sized to the client", || {
        c.get_pane(pane).unwrap().size_cells == (60, 20)
    });
    t.master
        .resize(PtySize {
            rows: 40,
            cols: 120,
            pixel_width: 0,
            pixel_height: 0,
        })
        .unwrap();
    wait_for("sigwinch", || {
        c.get_pane(pane).unwrap().size_cells == (120, 40)
    });
    t.type_bytes(&[0x02, b'd']);
    assert_eq!(t.exit_code(), 0);
}

#[test]
fn the_picker_lists_sessions_and_enter_attaches() {
    let e = env("pty-picker");
    make(&e.h, "alpha");
    let bravo = make(&e.h, "bravo");
    let c = Client::connect(e.h.socket()).unwrap();
    let mut t = Term::spawn(&[], e.h.socket(), e.home.path(), (90, 25));
    wait_for("picker", || {
        let s = t.text();
        s.contains("alpha") && s.contains("bravo") && s.contains("tear sessions")
    });
    t.type_bytes(b"brav");
    t.type_bytes(b"\r");
    wait_for("attached", || c.pane_subscriber_count(bravo).unwrap() == 1);
    t.type_bytes(b"echo HOP$((6*7))\r");
    wait_for("output", || t.text().contains("HOP42"));
    t.type_bytes(&[0x02, b'd']);
    assert_eq!(t.exit_code(), 0);
}

#[test]
fn ctrl_s_inside_a_session_hops_to_another_without_leaving() {
    let e = env("pty-hop");
    let alpha = make(&e.h, "alpha");
    let bravo = make(&e.h, "bravo");
    let c = Client::connect(e.h.socket()).unwrap();
    let mut t = Term::spawn(&["alpha"], e.h.socket(), e.home.path(), (90, 25));
    wait_for("attached to alpha", || {
        c.pane_subscriber_count(alpha).unwrap() == 1
    });
    t.type_bytes(&[0x13]);
    wait_for("overlay", || t.text().contains("tear sessions"));
    t.type_bytes(b"brav\r");
    wait_for("attached to bravo", || {
        c.pane_subscriber_count(bravo).unwrap() == 1
    });
    t.type_bytes(b"echo ON$((1+2))\r");
    wait_for("hopped", || t.text().contains("ON3"));
    t.type_bytes(&[0x02, b'd']);
    assert_eq!(t.exit_code(), 0);
    let c = Client::connect(e.h.socket()).unwrap();
    assert_eq!(c.list_sessions().unwrap().len(), 2);
}

#[test]
fn a_custom_picker_key_and_detach_chord_come_from_the_config() {
    let e = env("pty-config");
    let alpha = make(&e.h, "alpha");
    write_config(
        &e,
        "attach:\n  picker_key: ctrl+o\n  detach_prefix: ctrl+a\n  detach_key: q\n",
    );
    let c = Client::connect(e.h.socket()).unwrap();
    let mut t = Term::spawn(&["alpha"], e.h.socket(), e.home.path(), (90, 25));
    wait_for("attached", || c.pane_subscriber_count(alpha).unwrap() == 1);
    t.type_bytes(&[0x0f]);
    wait_for("overlay on ctrl+o", || t.text().contains("tear sessions"));
    let before = t.text().matches("\x1b[?1049l").count();
    t.type_bytes(&[0x1b]);
    wait_for("back in the pane", || {
        t.text().matches("\x1b[?1049l").count() > before
    });
    t.type_bytes(&[0x01, b'q']);
    assert_eq!(t.exit_code(), 0);
}

#[test]
fn a_refused_picker_key_stops_attach_with_a_clear_message() {
    let e = env("pty-refused");
    make(&e.h, "alpha");
    write_config(&e, "attach:\n  picker_key: ctrl+c\n");
    let mut t = Term::spawn(&["alpha"], e.h.socket(), e.home.path(), (90, 25));
    assert_ne!(t.exit_code(), 0);
    let s = t.text();
    assert!(s.contains("refuse list"), "{s}");
    assert!(!s.contains("panicked"), "{s}");
}

#[test]
fn attach_can_be_switched_off_and_says_so() {
    let e = env("pty-off");
    make(&e.h, "alpha");
    write_config(&e, "attach:\n  enabled: false\n");
    let mut t = Term::spawn(&["alpha"], e.h.socket(), e.home.path(), (90, 25));
    assert_ne!(t.exit_code(), 0);
    assert!(t.text().contains("switched off"));
}

#[test]
fn an_unreachable_daemon_is_a_message_not_a_panic() {
    let e = env("pty-dead");
    let gone = e.home.path().join("nobody.sock");
    let mut t = Term::spawn(&["work"], &gone, e.home.path(), (90, 25));
    assert_ne!(t.exit_code(), 0);
    let s = t.text();
    assert!(s.contains("not reachable"), "{s}");
    assert!(!s.contains("panicked"), "{s}");
}

#[test]
fn an_unknown_session_is_a_message_not_a_panic() {
    let e = env("pty-unknown");
    make(&e.h, "alpha");
    let mut t = Term::spawn(&["zzz"], e.h.socket(), e.home.path(), (90, 25));
    assert_ne!(t.exit_code(), 0);
    assert!(t.text().contains("no session matches"));
}

#[test]
fn no_tty_is_a_message_not_a_panic() {
    let e = env("pty-notty");
    let out = std::process::Command::new(env!("CARGO_BIN_EXE_tear"))
        .args(["attach", "--socket"])
        .arg(e.h.socket())
        .env("HOME", e.home.path())
        .env("XDG_CONFIG_HOME", e.home.path().join("config"))
        .stdin(std::process::Stdio::null())
        .output()
        .unwrap();
    assert!(!out.status.success());
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("needs a terminal"), "{err}");
    assert!(!err.contains("panicked"), "{err}");
}

#[test]
fn an_empty_daemon_shows_the_no_sessions_state() {
    let e = env("pty-empty");
    let mut t = Term::spawn(&[], e.h.socket(), e.home.path(), (90, 25));
    wait_for("empty picker", || t.text().contains("no sessions yet"));
    t.type_bytes(&[0x1b]);
    assert_eq!(t.exit_code(), 0);
}
