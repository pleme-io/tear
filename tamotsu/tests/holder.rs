use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use makimono::{Ending, JournalBounds, PaneDir};
use tamotsu::proto::{FromHolder, PROTO, ToHolder, read_from_holder, write_to_holder};
use tamotsu::{HeldPty, HoldArgs, HoldProgram, Revival};

struct Scratch(PathBuf);

impl Scratch {
    fn new(tag: &str) -> Self {
        let p = PathBuf::from("/tmp").join(format!("tmt-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&p);
        std::fs::create_dir_all(&p).unwrap();
        Self(p)
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn revival(dir: &Path) -> Revival {
    Revival {
        program: HoldProgram {
            program: PathBuf::from(env!("CARGO_BIN_EXE_tamotsu")),
            prefix: vec![],
        },
        args: HoldArgs {
            pane_dir: dir.join("pane"),
            socket: dir.join("h.sock"),
            cols: 80,
            rows: 24,
            cwd: Some(dir.to_string_lossy().into_owned()),
            resurrected: false,
            bounds: JournalBounds::default(),
            program: "/bin/sh".into(),
            args: vec![],
        },
        env: vec![
            ("PATH".into(), "/usr/bin:/bin".into()),
            ("PS1".into(), "$ ".into()),
            ("TERM".into(), "dumb".into()),
        ],
    }
}

type Seen = Arc<Mutex<Vec<u8>>>;

fn sinks() -> (
    Seen,
    tamotsu::OnBytes,
    mpsc::Receiver<Option<i32>>,
    tamotsu::OnExit,
) {
    let seen: Seen = Arc::default();
    let s = Arc::clone(&seen);
    let (tx, rx) = mpsc::channel();
    (
        seen,
        Box::new(move |b: &[u8]| s.lock().unwrap().extend_from_slice(b)),
        rx,
        Box::new(move |code| {
            let _ = tx.send(code);
        }),
    )
}

fn wait_for(seen: &Seen, needle: &str) -> bool {
    let until = Instant::now() + Duration::from_secs(10);
    while Instant::now() < until {
        if String::from_utf8_lossy(&seen.lock().unwrap()).contains(needle) {
            return true;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    false
}

fn child_pid(socket: &Path) -> Option<u32> {
    let mut s = UnixStream::connect(socket).ok()?;
    write_to_holder(&mut s, &ToHolder::Hello { proto: PROTO }).ok()?;
    match read_from_holder(&mut s).ok()? {
        FromHolder::Hello { child_pid, .. } => child_pid,
        _ => None,
    }
}

fn holder_pid(socket: &Path) -> Option<i32> {
    let mut s = UnixStream::connect(socket).ok()?;
    write_to_holder(&mut s, &ToHolder::Hello { proto: PROTO }).ok()?;
    match read_from_holder(&mut s).ok()? {
        FromHolder::Hello { pid, .. } => i32::try_from(pid).ok(),
        _ => None,
    }
}

#[test]
fn a_detached_session_keeps_its_process_and_replays_its_history_on_adoption() {
    let t = Scratch::new("adopt");
    let rv = revival(&t.0);
    let (seen, on_bytes, _exit, on_exit) = sinks();
    let held = HeldPty::launch(rv.clone(), on_bytes, on_exit).unwrap();
    held.write(b"echo first-$((40+2))\n").unwrap();
    assert!(wait_for(&seen, "first-42"));
    let pid_before = child_pid(&rv.args.socket).expect("holder answers");
    drop(held);
    std::thread::sleep(Duration::from_millis(200));
    assert!(
        HeldPty::probe(&rv.args.socket),
        "detaching must not end the holder"
    );

    let (seen2, on_bytes2, exit2, on_exit2) = sinks();
    let adopted = HeldPty::adopt(rv.clone(), on_bytes2, on_exit2).unwrap();
    assert!(wait_for(&seen2, "first-42"), "adoption replays the journal");
    assert_eq!(
        child_pid(&rv.args.socket),
        Some(pid_before),
        "the same shell survived"
    );
    adopted.write(b"echo second-$((1+1))\n").unwrap();
    assert!(wait_for(&seen2, "second-2"));

    adopted.end();
    assert_eq!(exit2.recv_timeout(Duration::from_secs(10)).unwrap(), None);
    let tomb = PaneDir::at(&rv.args.pane_dir).tombstone().unwrap();
    assert!(matches!(tomb, Some(Ending::EndedBy { .. })), "{tomb:?}");
    assert!(!HeldPty::probe(&rv.args.socket));
}

#[test]
fn a_shell_that_exits_writes_its_exit_code_as_the_ending() {
    let t = Scratch::new("exit");
    let rv = revival(&t.0);
    let (_seen, on_bytes, exit, on_exit) = sinks();
    let held = HeldPty::launch(rv.clone(), on_bytes, on_exit).unwrap();
    held.write(b"exit 7\n").unwrap();
    assert_eq!(exit.recv_timeout(Duration::from_secs(10)).unwrap(), Some(7));
    assert_eq!(
        PaneDir::at(&rv.args.pane_dir).tombstone().unwrap(),
        Some(Ending::Exited { code: Some(7) })
    );
}

#[test]
fn a_killed_holder_is_revived_in_place_with_a_new_shell_and_its_scrollback() {
    let t = Scratch::new("revive");
    let rv = revival(&t.0);
    let (seen, on_bytes, exit, on_exit) = sinks();
    let held = HeldPty::launch(rv.clone(), on_bytes, on_exit).unwrap();
    held.write(b"echo before-$((3*3))\n").unwrap();
    assert!(wait_for(&seen, "before-9"));
    let old_shell = child_pid(&rv.args.socket).unwrap();
    let hp = holder_pid(&rv.args.socket).unwrap();
    nix::sys::signal::kill(
        nix::unistd::Pid::from_raw(hp),
        nix::sys::signal::Signal::SIGKILL,
    )
    .unwrap();
    assert!(
        wait_for(&seen, "session resurrected"),
        "the revival is visible in the stream"
    );
    let until = Instant::now() + Duration::from_secs(10);
    let mut wrote = false;
    while Instant::now() < until && !wrote {
        wrote = held.write(b"echo after-$((5*5))\n").is_ok();
        std::thread::sleep(Duration::from_millis(50));
    }
    assert!(wrote);
    assert!(wait_for(&seen, "after-25"));
    let new_shell = child_pid(&rv.args.socket).unwrap();
    assert_ne!(
        new_shell, old_shell,
        "a revival is a new incarnation, never a claimed survivor"
    );
    let journal = PaneDir::at(&rv.args.pane_dir)
        .open_journal(JournalBounds::default())
        .unwrap()
        .read_all()
        .unwrap();
    let text = String::from_utf8_lossy(&journal);
    assert!(
        text.contains("before-9") && text.contains("after-25"),
        "{text}"
    );
    held.end();
    assert_eq!(exit.recv_timeout(Duration::from_secs(10)).unwrap(), None);
    assert!(
        !HeldPty::probe(&rv.args.socket),
        "the revived holder is gone"
    );
}
