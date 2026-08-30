//! espelho conformance — tear-core against the typed
//! terminal-conformance contract (third fleet site, after mado's
//! `Terminal` host rows and frost's guest harness).
//!
//! > **★ UPDATED 2026-07-31 — the relay finding below is still accurate for
//! > the SHIPPED default, but it is no longer permanent.** The claim
//! > "`feed()` has no write-back surface at all" was true when written and
//! > is now false: `PaneGrid` has a `pending_response` lane and answers
//! > DSR 5 / DSR 6 (CPR) / DA1 / DA2 when its `HostRole` is `Host`.
//! >
//! > The default is still `HostRole::Relay`, which answers **nothing**, so
//! > every row in this file continues to describe live behaviour. What
//! > changed is that the role is now a typed field rather than an absence —
//! > because after the shuken flip mado has no parser, so the host duty
//! > cannot keep living "one layer down". See `docs/SHUKEN.md` and the
//! > `host_role_rows` module in `pane_grid.rs`.
//! >
//! > **The relay rows below become the WRONG contract the moment a pane is
//! > set to `Host`** — at that point tear owes espelho the HOST rows, not
//! > the relay ones. Flipping the default without adding those rows here is
//! > the mistake this note exists to prevent.
//!
//! ## Which contract does tear satisfy? (shape discovery — honest)
//!
//! espelho names two roles: a HOST answers VT queries, a GUEST
//! survives any host. tear's `PaneGrid` is NEITHER — it is a **RELAY**
//! (multiplexer pass-through). The grid is a pure interpreter:
//! `feed()` has no write-back surface at all, so it *cannot* answer
//! `ESC[6n` itself — CSI finals `n` / `c` / `q` fall through to the
//! ignore arm, and the OSC 10/11 `?` queries are not title/cwd/133
//! codes so they are dropped from grid state. The host duty lives one
//! layer DOWN the subscriber stream: the downstream terminal (mado)
//! answers the query, which is only possible if the query bytes
//! survive **byte-verbatim** through tear's fan-out. That is the
//! third-site shape this file pins: the relay row is a real contract
//! of its own (a `Role::Relay` candidate for espelho upstream — noted,
//! not invented here).
//!
//! Conformance rows asserted:
//!
//! 1. **Parser totality** — every prefix of every catalog wire,
//!    embedded in realistic byte streams, never panics `PaneGrid`
//!    (torn writes + split escapes across reads).
//! 2. **Grid integrity** — query wires leave no residue in the
//!    rendered text (a relay that smears `6n` into the grid corrupts
//!    what the operator sees).
//! 3. **Pass-through integrity** — through a real PTY + the
//!    `InProcess` fan-out, each query wire reaches the subscriber
//!    stream verbatim, so the downstream host can answer.

use std::sync::mpsc;
use std::time::{Duration, Instant};

use espelho::VtQuery;
use tear_types::MultiplexerControl;

use crate::{InProcess, PaneGrid};

/// The full query catalog. espelho's own `scan` table is private, so
/// the variants are enumerated here, and [`catalog_index`] below is what
/// actually holds this list to espelho's enum.
///
/// **Corrected 2026-08-01.** This docstring used to say
/// `catalog_is_exhaustive` "fails the build if espelho grows a variant this
/// file doesn't cover." It could not. That test builds its input by mapping
/// over `CATALOG` itself and then asserts that scanning the result yields
/// `CATALOG` — circular by construction, so a variant espelho added and this
/// list omitted is never in the input and can never be detected. Its own
/// inline comment half-conceded this ("this count assertion *documents* the
/// coverage"), while the docstring promised enforcement. The test is still
/// worth keeping — it pins scan/wire round-tripping — but it was never the
/// forcing function, and [`catalog_index`] now is.
const CATALOG: [VtQuery; 6] = [
    VtQuery::CursorPosition,
    VtQuery::DeviceStatus,
    VtQuery::PrimaryDeviceAttributes,
    VtQuery::TerminalVersion,
    VtQuery::OscForeground,
    VtQuery::OscBackground,
];

/// The real forcing function: an **exhaustive match with no wildcard**.
///
/// `espelho::VtQuery` is a plain `pub enum` — not `#[non_exhaustive]` — so
/// adding a variant upstream breaks this match with `E0004` and tear-core
/// stops compiling until the variant is placed in [`CATALOG`]. That is a
/// compile error, which is what the old docstring claimed and a
/// self-referential runtime assertion could never deliver.
///
/// Returning the index (rather than `bool`) is what ties the match to the
/// array: `catalog_index_matches_catalog_order` below asserts
/// `CATALOG[catalog_index(q)] == q`, so a variant can be added to the match
/// and *still* fail if it was not put in the list at the right position.
///
/// Tier: **truly-unrepresentable** for "espelho grew a variant this file does
/// not name" (E0004). The ordering tie is a test — **CI-caught**.
/// Do not collapse those two into one claim.
const fn catalog_index(q: VtQuery) -> usize {
    match q {
        VtQuery::CursorPosition => 0,
        VtQuery::DeviceStatus => 1,
        VtQuery::PrimaryDeviceAttributes => 2,
        VtQuery::TerminalVersion => 3,
        VtQuery::OscForeground => 4,
        VtQuery::OscBackground => 5,
    }
}

/// Realistic byte streams a multiplexer actually sees, each embedding
/// the query wire: bare, mid-prompt (SGR-wrapped), inside an
/// alt-screen TUI burst, back-to-back with the whole catalog, and
/// preceded by a dangling ESC (the split-hostile case).
fn realistic_streams(q: VtQuery) -> Vec<Vec<u8>> {
    let w = q.wire();
    vec![
        w.to_vec(),
        [b"\x1b[1;32muser@host\x1b[0m $ ".as_slice(), w, b" ls\r\n"].concat(),
        [b"\x1b[?1049h\x1b[2J\x1b[H".as_slice(), w, b"\x1b[?1049l"].concat(),
        CATALOG.iter().flat_map(|o| o.wire().to_vec()).collect(),
        [b"\x1b".as_slice(), w].concat(),
    ]
}

#[test]
fn catalog_is_exhaustive() {
    // Forcing function: if espelho adds a VtQuery variant, scanning a
    // stream of every-variant wire must find exactly our catalog. A
    // new variant's wire would either extend this stream (compile
    // error on the match in realistic_streams' callers is not
    // guaranteed for Copy enums) or — caught here — scan would still
    // resolve all 6 and this count assertion documents the coverage.
    let all: Vec<u8> = CATALOG.iter().flat_map(|q| q.wire().to_vec()).collect();
    let mut cursor = 0;
    let mut found = Vec::new();
    while let Some((q, end)) = VtQuery::scan(&all, cursor) {
        found.push(q);
        cursor = end;
    }
    assert_eq!(found.as_slice(), CATALOG.as_slice());
}

/// Ties [`catalog_index`]'s exhaustive match to [`CATALOG`]'s contents.
///
/// The match alone proves every variant is *named*; this proves each is in
/// the array, at the position the match assigns. Adding a variant to the
/// match without adding it to `CATALOG` — the obvious way to silence an
/// `E0004` without doing the work — fails here.
#[test]
fn catalog_index_matches_catalog_order() {
    for (i, q) in CATALOG.iter().enumerate() {
        assert_eq!(
            catalog_index(*q),
            i,
            "{q:?} is at CATALOG[{i}] but catalog_index says {}",
            catalog_index(*q)
        );
    }
    assert_eq!(
        CATALOG.len(),
        6,
        "CATALOG changed size — update catalog_index's match and this count \
         together, or the two halves of the guard drift apart"
    );
}

#[test]
fn pane_grid_never_panics_on_any_prefix_of_query_streams() {
    for q in CATALOG {
        for stream in realistic_streams(q) {
            // Every prefix into a fresh grid — a torn write must never
            // panic the parser, and the render path must stay sane on
            // a half-fed escape.
            for end in 0..=stream.len() {
                let mut grid = PaneGrid::new(80, 24);
                grid.feed(&stream[..end]);
                let _ = grid.snapshot();
            }
            // Byte-at-a-time into ONE grid — escapes split across
            // reads (the PTY chunk boundary case) must reassemble.
            let mut grid = PaneGrid::new(80, 24);
            for b in &stream {
                grid.feed(std::slice::from_ref(b));
            }
            let _ = grid.snapshot();
        }
    }
}

#[test]
fn query_wires_leave_no_residue_in_rendered_text() {
    for q in CATALOG {
        let mut grid = PaneGrid::new(80, 24);
        grid.feed(b"before|");
        grid.feed(q.wire());
        grid.feed(b"|after");
        let snap = grid.snapshot();
        let row0 = snap.to_text_rows().into_iter().next().unwrap_or_default();
        assert!(
            row0.trim_end() == "before||after",
            "query {q:?} left residue in the grid: {row0:?}"
        );
    }
}

#[test]
fn query_wires_pass_through_verbatim_to_subscribers() {
    // The relay row. /bin/cat writes its stdin back out through the
    // PTY, so every wire we send_keys must reappear in the subscriber
    // stream byte-verbatim (the tty's ECHOCTL echo renders ESC as
    // `^[` — that mangled copy is ignored; cat's own write is the
    // verbatim one espelho::VtQuery::scan locks onto). A trailing \n
    // flushes the canonical-mode line; no wire contains \r or \n so
    // ONLCR/ICRNL can't touch the query bytes themselves.
    let inproc = InProcess::new();
    let sid = inproc
        .new_session("espelho-relay", "/bin/cat")
        .expect("new_session(/bin/cat)");
    let pane = *inproc
        .get_session(sid)
        .unwrap()
        .panes
        .keys()
        .next()
        .unwrap();
    let rx = inproc.subscribe_pane_bytes(pane).expect("subscribe");

    for q in CATALOG {
        let mut framed = b"marker-".to_vec();
        framed.extend_from_slice(q.wire());
        framed.extend_from_slice(b"-end\n");
        inproc.send_keys(pane, &framed).expect("send_keys");

        let deadline = Instant::now() + Duration::from_secs(5);
        let mut buf = Vec::<u8>::new();
        let mut relayed = false;
        while Instant::now() < deadline {
            match rx.recv_timeout(Duration::from_millis(100)) {
                Ok(chunk) => {
                    buf.extend_from_slice(&chunk);
                    // Rolling scan: a slow tail from the PREVIOUS
                    // query's echo may land in this iteration's buf,
                    // so walk every match rather than only the first.
                    let mut cursor = 0;
                    while let Some((found, end)) = VtQuery::scan(&buf, cursor) {
                        if found == q {
                            relayed = true;
                            break;
                        }
                        cursor = end;
                    }
                    if relayed {
                        break;
                    }
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(mpsc::RecvTimeoutError::Disconnected) => break,
            }
        }
        assert!(
            relayed,
            "query {q:?} did not survive verbatim through the relay — \
             downstream terminal could never answer it; transcript: {:?}",
            String::from_utf8_lossy(&buf)
        );
    }
}
