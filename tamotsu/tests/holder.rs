use std::time::{Duration, Instant};

use makimono::{Ending, JournalBounds, PaneDir};
use tamotsu::HeldPty;

#[allow(dead_code)]
mod common;

use common::*;

#[test]
fn a_detached_session_keeps_its_process_and_replays_its_history_on_adoption() {
    let t = Scratch::new("adopt");
    let rv = revival(&t.0);
    let (seen, on_bytes, _exit, on_exit) = sinks();
    let lease = authority(&t.0);
    let held = HeldPty::launch(rv.clone(), &lease, on_bytes, on_exit).unwrap();
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
    let adopted = HeldPty::adopt(rv.clone(), &lease, on_bytes2, on_exit2).unwrap();
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
    assert!(gone(&rv.args.socket));
}

#[test]
fn a_shell_that_exits_writes_its_exit_code_as_the_ending() {
    let t = Scratch::new("exit");
    let rv = revival(&t.0);
    let (_seen, on_bytes, exit, on_exit) = sinks();
    let held = HeldPty::launch(rv.clone(), &authority(&t.0), on_bytes, on_exit).unwrap();
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
    let held = HeldPty::launch(rv.clone(), &authority(&t.0), on_bytes, on_exit).unwrap();
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
    assert!(gone(&rv.args.socket), "the revived holder is gone");
}
