//! Does `set_spawn_env(cwd)` actually reach every pane of a multi-pane
//! `instantiate`, and does the window in which it can race actually exist?
//!
//! Two readings of the same code were in conflict and both cited real lines:
//!
//! * "set the global `SpawnEnv` before `instantiate` and all three panes land
//!   in the PR checkout" — citing `inproc.rs:508` (session spawn) and
//!   `inproc.rs:2453` (`split_pane_yurai`), which both read `spawn_env.cwd`.
//! * "not viable, it races" — citing `registry.rs:110-113`, where tear
//!   *deliberately declines* to read `spawn_env` for window-birth provenance:
//!   *"an RwLock shared across every connection and already races: a raced cwd
//!   is a wrong directory."*
//!
//! They are not actually contradictory, and guessing which one governs is how
//! a PR-review archetype ships panes in the wrong directory. So: measure.
//!
//! These tests answer the two halves separately, because the answers differ
//! and conflating them is the whole trap.

use tear_core::inproc::InProcess;
use tear_types::{Direction, MultiplexerControl, SpawnEnv};

/// Build a 2-pane session under a given `spawn_env` cwd and report the cwd
/// each pane actually got.
fn cwds_under(cwd: &str) -> Vec<Option<String>> {
    let inproc = InProcess::new();
    let mut env = SpawnEnv::none();
    env.cwd = Some(cwd.to_owned());
    inproc.set_spawn_env(env);

    let sid = inproc.new_session("probe", "/bin/sh").unwrap();
    let s0 = inproc.get_session(sid).unwrap();
    let p0 = s0.windows[&s0.active_window].active_pane;
    inproc
        .split_pane(p0, Direction::Right, "/bin/sh", &[])
        .unwrap();

    let session = inproc.get_session(sid).unwrap();
    let mut out: Vec<Option<String>> = session.panes.values().map(|p| p.cwd.clone()).collect();
    out.sort();
    out
}

/// HALF ONE — propagation. The session pane AND the split pane both pick up
/// the global cwd. This is the half the "it works today" reading got right,
/// and it is why the archetype does not need tear's per-pane cwd parameter
/// for a PR session, where all three panes want the same directory anyway.
#[test]
fn spawn_env_cwd_reaches_both_the_session_pane_and_a_split() {
    let got = cwds_under("/tmp/probe-alpha");
    assert_eq!(got.len(), 2, "one session pane + one split");
    for c in &got {
        assert_eq!(
            c.as_deref(),
            Some("/tmp/probe-alpha"),
            "every pane must inherit the global spawn cwd; got {got:?}"
        );
    }
}

/// HALF TWO — the race window is REAL, and this is the half the "it works"
/// reading missed.
///
/// `spawn_env` is read once per spawn, not captured once per instantiate. A
/// writer landing between two spawns retargets only the later panes, so a
/// 3-pane archetype can end up straddling two directories with no error
/// anywhere. This test performs that interleave deterministically — no
/// threads, no timing — because the point is that the window is structural,
/// not that it is likely.
///
/// Consequence for the archetype: setting the global env before
/// `instantiate` is CORRECT but not ATOMIC. It is safe when the caller owns
/// the only writer for the duration; it is not safe as a general mechanism,
/// which is exactly why `registry.rs:110-113` refuses to depend on it and why
/// mado's MCP path declines the same trick.
#[test]
fn a_write_between_spawns_splits_one_session_across_two_directories() {
    let inproc = InProcess::new();

    let mut first = SpawnEnv::none();
    first.cwd = Some("/tmp/probe-first".to_owned());
    inproc.set_spawn_env(first);

    let sid = inproc.new_session("raced", "/bin/sh").unwrap();
    let s0 = inproc.get_session(sid).unwrap();
    let p0 = s0.windows[&s0.active_window].active_pane;

    // The interleave: any other writer — an auto-attach cd, an MCP spawn —
    // between the session spawn and the split.
    let mut second = SpawnEnv::none();
    second.cwd = Some("/tmp/probe-second".to_owned());
    inproc.set_spawn_env(second);

    inproc
        .split_pane(p0, Direction::Right, "/bin/sh", &[])
        .unwrap();

    let session = inproc.get_session(sid).unwrap();
    let mut got: Vec<Option<String>> = session.panes.values().map(|p| p.cwd.clone()).collect();
    got.sort();

    assert_eq!(
        got,
        vec![
            Some("/tmp/probe-first".to_owned()),
            Some("/tmp/probe-second".to_owned())
        ],
        "the race is structural: one session, two directories, no error raised"
    );
}
