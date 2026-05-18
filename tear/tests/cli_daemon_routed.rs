//! Cross-process integration test: every CLI subcommand that the
//! "M2 daemon-routed" pass promoted (`up`, `list`, `kill`,
//! `rename`, `status`) is exercised against a real in-test
//! `tear-daemon`.
//!
//! Choreography for each test:
//!   1. Spin up `tear_daemon::start(socket, InProcess)` in-process
//!      on a private UDS.
//!   2. Invoke the production tear binary via
//!      `env!("CARGO_BIN_EXE_tear")` with `--socket <sock>`.
//!   3. Assert on stdout / stderr / exit code.
//!
//! Proves the CLI surface mado MCP and any other operator-facing
//! caller depends on actually talks to the daemon — no more
//! "session lives in this CLI process, dies on exit" M0 behaviour.

use std::process::{Command, Stdio};
use std::sync::Arc;
use std::time::Duration;

/// Per-test daemon scaffold. Drop stops the daemon and unlinks
/// the socket. Each test gets a unique PID+counter socket so
/// parallel `cargo test` workers don't collide.
struct DaemonHarness {
    socket: std::path::PathBuf,
    daemon: Option<tear_daemon::DaemonHandle>,
}

impl DaemonHarness {
    fn new(label: &str) -> Self {
        use std::sync::atomic::{AtomicU32, Ordering};
        static SEQ: AtomicU32 = AtomicU32::new(0);
        let pid = std::process::id();
        let seq = SEQ.fetch_add(1, Ordering::Relaxed);
        let mut socket = std::env::temp_dir();
        socket.push(format!("tear-cli-{label}-{pid}-{seq}.sock"));
        let _ = std::fs::remove_file(&socket);
        let inproc = Arc::new(tear_core::InProcess::new());
        let daemon =
            tear_daemon::start(socket.clone(), inproc).expect("daemon start");
        std::thread::sleep(Duration::from_millis(50));
        Self {
            socket,
            daemon: Some(daemon),
        }
    }

    /// Build a `Command` for the production tear binary, already
    /// targeted at this harness's socket.
    fn cmd(&self) -> Command {
        let bin = env!("CARGO_BIN_EXE_tear");
        let mut c = Command::new(bin);
        c.arg("--").stdin(Stdio::null()); // clap eats the `--`; placeholder
        // start over without the placeholder — clap parses cleanly
        let mut c = Command::new(bin);
        c.stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        c
    }

    /// Convenience: run `tear <args>` and capture (stdout, stderr,
    /// exit_code). The args list does NOT include the binary path
    /// or the `--socket <path>` flag — those are appended.
    fn run(&self, args: &[&str]) -> (String, String, i32) {
        let mut cmd = self.cmd();
        cmd.args(args);
        // The Status subcommand has --socket placed first; List /
        // Up / Kill / Rename accept it after the subcommand. Just
        // append it; clap's per-subcommand arg parser handles it.
        cmd.arg("--socket").arg(&self.socket);
        let out = cmd.output().expect("spawn tear");
        (
            String::from_utf8_lossy(&out.stdout).into_owned(),
            String::from_utf8_lossy(&out.stderr).into_owned(),
            out.status.code().unwrap_or(-1),
        )
    }
}

impl Drop for DaemonHarness {
    fn drop(&mut self) {
        if let Some(d) = self.daemon.take() {
            d.stop();
        }
    }
}

// ── tear status ────────────────────────────────────────────────────

#[test]
fn status_against_running_daemon_reports_reachable() {
    let h = DaemonHarness::new("status-ok");
    let (stdout, _stderr, code) = h.run(&["status"]);
    assert_eq!(code, 0, "status should exit 0 when daemon reachable");
    assert!(
        stdout.contains("tear-daemon: ok"),
        "expected ok line, got: {stdout}"
    );
    assert!(stdout.contains("sessions=0"));
}

#[test]
fn status_against_running_daemon_emits_parseable_json() {
    let h = DaemonHarness::new("status-json");
    let (stdout, _stderr, code) = h.run(&["status", "--json"]);
    assert_eq!(code, 0);
    let v: serde_json::Value = serde_json::from_str(stdout.trim())
        .unwrap_or_else(|e| panic!("bad json: {e}\nraw: {stdout}"));
    assert_eq!(v["reachable"], true);
    assert_eq!(v["sessions"], 0);
    assert!(v["version"].is_string());
    assert!(v["socket"].as_str().unwrap().ends_with(".sock"));
}

#[test]
fn status_when_daemon_down_exits_nonzero() {
    // Pick a socket we know nobody is listening on.
    let mut socket = std::env::temp_dir();
    socket.push(format!("tear-status-down-{}.sock", std::process::id()));
    let _ = std::fs::remove_file(&socket);

    let bin = env!("CARGO_BIN_EXE_tear");
    let out = Command::new(bin)
        .args(["status", "--socket"])
        .arg(&socket)
        .output()
        .expect("spawn");
    assert_eq!(out.status.code(), Some(1));
}

#[test]
fn status_quiet_mode_suppresses_output_but_keeps_exit_code() {
    let h = DaemonHarness::new("status-quiet");
    let (stdout, stderr, code) = h.run(&["status", "--quiet"]);
    assert_eq!(code, 0);
    assert!(stdout.is_empty(), "quiet should suppress stdout: {stdout:?}");
    assert!(stderr.is_empty(), "quiet should suppress stderr: {stderr:?}");
}

// ── tear up + list + kill (daemon-routed) ──────────────────────────

#[test]
fn up_creates_session_in_daemon_visible_via_list() {
    let h = DaemonHarness::new("up-list");
    let (stdout, _stderr, code) =
        h.run(&["up", "--name", "lifecycle", "--shell", "/bin/sh"]);
    assert_eq!(code, 0, "up failed: {stdout}");
    assert!(stdout.contains("created session"));
    assert!(stdout.contains("(lifecycle) in daemon"));

    let (list_stdout, _se, list_code) = h.run(&["list"]);
    assert_eq!(list_code, 0);
    assert!(
        list_stdout.contains("lifecycle"),
        "list missed the session: {list_stdout}"
    );
}

#[test]
fn up_list_yaml_emits_round_trippable_yaml() {
    let h = DaemonHarness::new("up-list-yaml");
    let _ = h.run(&["up", "--name", "yaml-test", "--shell", "/bin/sh"]);
    let (stdout, _se, code) = h.run(&["list", "--yaml"]);
    assert_eq!(code, 0);
    let parsed: serde_yaml_ng::Value = serde_yaml_ng::from_str(&stdout)
        .unwrap_or_else(|e| panic!("bad yaml: {e}\nraw: {stdout}"));
    let arr = parsed.as_sequence().expect("yaml top-level is a sequence");
    assert_eq!(arr.len(), 1);
    assert_eq!(arr[0]["name"], serde_yaml_ng::Value::from("yaml-test"));
}

#[test]
fn kill_by_id_removes_session() {
    let h = DaemonHarness::new("kill-id");
    let (up_stdout, _, _) = h.run(&["up", "--name", "kill-me"]);
    // up output: "created session <id> (kill-me) in daemon"
    // up output may have a leading tracing line; grep the
    // "created session <id> (<name>) in daemon" line and pull
    // the 3rd whitespace token off it.
    let sid = up_stdout
        .lines()
        .find(|l| l.contains("created session"))
        .and_then(|l| l.split_whitespace().nth(2))
        .unwrap_or_else(|| panic!("no session id in up output:\n{up_stdout}"));
    let (kill_stdout, _se, kill_code) = h.run(&["kill", sid]);
    assert_eq!(kill_code, 0, "kill failed: {kill_stdout}");
    assert!(kill_stdout.contains("killed session"));
    let (list_stdout, _se, _) = h.run(&["list"]);
    assert!(
        !list_stdout.contains(sid),
        "kill didn't actually remove the session: {list_stdout}"
    );
}

#[test]
fn kill_by_name_resolves_unique_match() {
    let h = DaemonHarness::new("kill-name");
    let _ = h.run(&["up", "--name", "unique-target"]);
    let (stdout, _se, code) = h.run(&["kill", "--name", "unique-target"]);
    assert_eq!(code, 0, "kill --name failed: {stdout}");
    assert!(stdout.contains("killed session"));
}

#[test]
fn kill_by_name_fails_when_no_match() {
    let h = DaemonHarness::new("kill-name-miss");
    let (_, stderr, code) =
        h.run(&["kill", "--name", "this-name-does-not-exist"]);
    assert_ne!(code, 0);
    assert!(
        stderr.contains("no session named"),
        "expected miss error, got: {stderr}"
    );
}

// ── tear rename (daemon-routed) ────────────────────────────────────

#[test]
fn rename_relabels_session_in_daemon_list() {
    let h = DaemonHarness::new("rename");
    let (up_stdout, _, _) = h.run(&["up", "--name", "before-rename"]);
    // up output may have a leading tracing line; grep the
    // "created session <id> (<name>) in daemon" line and pull
    // the 3rd whitespace token off it.
    let sid = up_stdout
        .lines()
        .find(|l| l.contains("created session"))
        .and_then(|l| l.split_whitespace().nth(2))
        .unwrap_or_else(|| panic!("no session id in up output:\n{up_stdout}"));

    let (stdout, _se, code) = h.run(&["rename", sid, "after-rename"]);
    assert_eq!(code, 0, "rename failed: {stdout}");
    assert!(stdout.contains("after-rename"));

    let (list_stdout, _se, _) = h.run(&["list"]);
    assert!(
        list_stdout.contains("after-rename"),
        "rename didn't propagate: {list_stdout}"
    );
    assert!(
        !list_stdout.contains("before-rename"),
        "old name still visible: {list_stdout}"
    );
}

// ── error paths shared by every daemon-routed command ──────────────

#[test]
fn up_against_missing_daemon_emits_hint() {
    let mut socket = std::env::temp_dir();
    socket.push(format!(
        "tear-cli-no-daemon-{}-{}.sock",
        std::process::id(),
        line!()
    ));
    let _ = std::fs::remove_file(&socket);
    let bin = env!("CARGO_BIN_EXE_tear");
    let out = Command::new(bin)
        .args(["up", "--name", "wont-bind", "--socket"])
        .arg(&socket)
        .output()
        .expect("spawn");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_ne!(out.status.code(), Some(0));
    assert!(
        stderr.contains("not reachable") || stderr.contains("Start it with"),
        "expected actionable hint, got: {stderr}"
    );
}
