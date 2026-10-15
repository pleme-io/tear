use std::os::unix::fs::PermissionsExt;
use std::process::{Command, Stdio};
use std::time::Duration;

use makimono::{JournalBounds, PaneDir};

#[allow(dead_code)]
mod common;

use common::*;

#[test]
fn a_holder_whose_log_and_journal_both_fail_keeps_streaming_the_pty() {
    let t = Scratch::new("stderr-gone");
    let mut rv = revival(&t.0);
    rv.args.bounds = JournalBounds {
        max_bytes: 1 << 20,
        segment_bytes: 1,
        fsync_interval_ms: 1000,
    };
    let pane = PaneDir::at(&rv.args.pane_dir);
    pane.create().unwrap();
    let mut child = Command::new(&rv.program.program)
        .args(&rv.program.prefix)
        .args(rv.args.to_argv())
        .env_clear()
        .envs(rv.env.iter().map(|(k, v)| (k.as_str(), v.as_str())))
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    drop(child.stderr.take());
    assert!(
        eventually(|| tamotsu::HeldPty::probe(&rv.args.socket)),
        "the holder never answered"
    );

    let mut raw = Raw::attached(&rv.args.socket, 0, Some(1));
    raw.write(b"echo before-$((6*7))\n");
    assert!(
        raw.until_text("before-42", Duration::from_secs(10)),
        "an attach whose log line cannot be written still streams"
    );

    let journal = rv.args.pane_dir.join("journal");
    std::fs::set_permissions(&journal, std::fs::Permissions::from_mode(0o500)).unwrap();
    for i in 0..20 {
        raw.write(format!("echo failing-{i}-$((1+1))\n").as_bytes());
    }
    let streamed = raw.until_text("failing-19-2", Duration::from_secs(10));
    std::fs::set_permissions(&journal, std::fs::Permissions::from_mode(0o700)).unwrap();
    assert!(
        streamed,
        "journal failures logged to a dead stderr must not stop the pump"
    );

    raw.write(b"echo after-$((2+2))\n");
    assert!(
        raw.until_text("after-4", Duration::from_secs(10)),
        "the pump outlives the failure"
    );
    let _ = child.kill();
    let _ = child.wait();
}
