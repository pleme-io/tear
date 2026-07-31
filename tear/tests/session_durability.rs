//! **A tear session outlives every client. That is the product.**
//!
//! This file is the seal on the 2026-07-31 defect: `tear-daemon`'s
//! "orphan pruner" killed any session with zero live byte-stream
//! subscribers after a grace keyed on provenance — 3 s for
//! `SessionSource::Named`, 15 s for `Agent`, never for `Human`. The
//! shells were still running. Nothing errored: every RPC returned `Ok`,
//! `get_session` succeeded, `send_keys` landed, and the session was gone
//! moments later. banken's bancada (`Named`) and mado's
//! `tear_new_session` MCP tool (`Agent`) were both bitten.
//!
//! What is asserted here is durability *and attachability*, not a
//! registry row: the creating client is **dropped entirely** (connection
//! closed, `Client` gone), a **fresh** connection is opened afterwards,
//! and the session's shell is proven to still be executing input typed
//! through that new connection. A row in `list_sessions` would not be
//! evidence — a session whose PTY was reaped but whose record lingered
//! would pass that and fail an operator.
//!
//! ## Fail-once (recorded 2026-07-31)
//!
//! Restoring the pruner thread in `tear-daemon/src/lib.rs` turns
//! `named_source_session_survives_its_creating_client` red on the
//! `sessions.len()` assertion with `left: 0, right: 3` — see the commit
//! message for the literal output.
//!
//! ## Timing, honestly
//!
//! [`ORPHANED_FOR`] is 8 s: past the deleted **3 s** `Named` grace even
//! at its worst-case tick alignment (the pruner ticked every 2 s and
//! only started counting on the first tick *after* creation, so a
//! `Named` session died at ~+6 s, and a 5 s wait measured GREEN against
//! the restored pruner — a fail-once that does not fail is worse than
//! none). It is deliberately short of the deleted **15 s** `Agent`
//! grace: the `Named` row is what makes this test red, and the `Agent`
//! and `Human` rows ride along as a provenance table rather than as
//! independently timed proofs. A ~22 s test to also time out the
//! `Agent` grace is not worth the wall-clock in every
//! `cargo test --workspace` run, and the *class* is sealed by
//! construction in `tear_core::reap` rather than by this timer.

use std::time::{Duration, Instant};

use tear_client::Client;
use tear_daemon::testing::DaemonHarness;
use tear_types::{MultiplexerControl, PaneState, SessionSource};

/// How long the sessions are left with no client attached at all.
/// See the module docs on what this does and does not cover.
const ORPHANED_FOR: Duration = Duration::from_secs(8);

/// Every provenance a consumer stamps, with the grace the deleted
/// pruner applied to it. Provenance is audit metadata for
/// `tear list --source`; it is not a lifetime, and none of these may
/// decide whether a live session lives.
fn every_provenance() -> Vec<(&'static str, SessionSource)> {
    vec![
        ("durable-human", SessionSource::Human),
        ("durable-agent", SessionSource::Agent),
        (
            "durable-named",
            SessionSource::Named("banken-bancada".to_owned()),
        ),
    ]
}

/// Poll a pane's rendered grid until it contains `needle`.
fn wait_for_text(client: &Client, pane: tear_types::PaneId, needle: &str, within: Duration) -> bool {
    let deadline = Instant::now() + within;
    while Instant::now() < deadline {
        if let Ok(snap) = client.pane_snapshot(pane) {
            if snap.to_text().contains(needle) {
                return true;
            }
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    false
}

#[test]
fn named_source_session_survives_its_creating_client() {
    let h = DaemonHarness::new("durability");

    // ── Client A creates the sessions, then ceases to exist. ─────
    let created: Vec<(String, tear_types::SessionId)> = {
        let a = Client::connect(h.socket()).expect("client A connect");
        let made = every_provenance()
            .into_iter()
            .map(|(name, source)| {
                let sid = a
                    .new_session_with_source_and_size(name, "/bin/sh", &[], source, (80, 24))
                    .unwrap_or_else(|e| panic!("create {name}: {e}"));
                (name.to_owned(), sid)
            })
            .collect::<Vec<_>>();
        // Sessions exist while their creator is still connected — the
        // defect got this far too, which is exactly why it was so
        // expensive to find.
        assert_eq!(a.list_sessions().expect("list from A").len(), 3);
        made
        // `a` drops here: the socket closes and the daemon's
        // per-connection thread winds down. Nothing is attached to any
        // of these sessions from this instant on.
    };

    std::thread::sleep(ORPHANED_FOR);

    // ── A FRESH connection. Nothing carried over from client A. ──
    let b = Client::connect(h.socket()).expect("client B connect");
    let sessions = b.list_sessions().expect("list from B");
    let surviving: Vec<&str> = sessions.iter().map(|s| s.name.as_str()).collect();
    assert_eq!(
        sessions.len(),
        3,
        "a session must outlive the client that created it — after \
         {ORPHANED_FOR:?} unattached, only {surviving:?} of {:?} remain",
        created.iter().map(|(n, _)| n.as_str()).collect::<Vec<_>>()
    );

    // ── Still ATTACHABLE, not merely still listed. ───────────────
    for (name, sid) in &created {
        let s = b
            .get_session(*sid)
            .unwrap_or_else(|e| panic!("{name} is listed but not gettable: {e}"));
        let pane = *s.panes.keys().next().expect("seed pane");
        assert_eq!(
            s.panes[&pane].state,
            PaneState::Running,
            "{name}'s shell must still be running"
        );
        // The real proof: the child process on the far side of the PTY
        // executes input typed through a connection its creator never
        // saw.
        let marker = format!("durable-{name}-ok");
        b.send_keys(pane, format!("echo {marker}\r").as_bytes())
            .unwrap_or_else(|e| panic!("send_keys to {name} after re-connect: {e}"));
        assert!(
            wait_for_text(&b, pane, &marker, Duration::from_secs(5)),
            "{name}'s shell never echoed {marker} — the session is listed \
             but not attachable"
        );
    }

    for (_, sid) in &created {
        let _ = b.kill_session(*sid);
    }
}
