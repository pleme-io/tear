# Session durability — a session ends only when it ends

> **Status: P1–P3 shipped 2026-10-06; P4 (fleet) ships with the nix
> wiring. §6 is the ledger, §7 records where the build departed from this
> design and why.** Designed 2026-09-22. This amends [`SESSION-TYPESCAPE.md`](./SESSION-TYPESCAPE.md)
> §2's `Phase-6` row and its illegal state #6, and says exactly which part of
> that entry was a fact about the world and which part was a fact about our
> own architecture.

## 0. The destination

A tear session ends for exactly two reasons:

1. **It ended on its own** — the shell (or the program the pane runs) exited.
2. **A human ended it** — an explicit kill, from the Ctrl-S picker, the CLI or
   an MCP call that names a human reason.

Every other way a session can stop is **involuntary** and is repaired, not
reported: a mado window closing, mado crashing, the tear daemon crashing or
being upgraded, the user logging out, the machine rebooting. The operator
leaves sessions in the background, comes back hours or reboots later, and they
are there.

The target is not "most sessions survive". It is that an involuntary end has
**no code path that finishes a session** — the only functions that write a
session's ending take one of the two causes above.

## 1. What was a world-fact, and what was ours

`SESSION-TYPESCAPE.md` makes "live PTYs survive a daemon restart"
unrepresentable (`Durability` has only `ProcessBound`). The *reason* it gives
is "the PTYs live in the daemon process". That is a statement about **our
process layout**, not about PTYs — MIRAGEM's test says it is ours to dissolve.
A PTY master fd is an ordinary file descriptor: whoever holds it keeps the
session alive, and it does not have to be the daemon.

What IS a fact about the world, and stays typed as a limit:

| World fact | Consequence |
|---|---|
| A reboot or logout kills every process of the user | processes cannot be carried across it; the session is **resurrected** (new shell, same cwd, persisted scrollback above it) — a *different* incarnation, and the type says so |
| launchd kills a job's process group when the job exits, unless `AbandonProcessGroup` | holders must not live in the daemon's job, or the plist must abandon the group |
| systemd `KillMode=control-group` kills the whole cgroup on stop/restart | the daemon unit needs `KillMode=process`, or holders run in their own scope |
| A kernel can SIGKILL anything (OOM) | a holder can still die; that is involuntary → resurrection from the journal |

## 2. The survival ladder

```
what dies                  today (Shipped)            destination
────────────────────────── ────────────────────────── ───────────────────────────────
a mado window              embedded: session dies     session lives, DETACHED
mado (exit / crash)        embedded: session dies     session lives, DETACHED
tear-daemon (crash/update) every shell dies           shell lives — tamotsu holds the PTY;
                                                      the new daemon re-adopts it
the holder (OOM, bug)      —                          RESURRECTED from makimono
logout / reboot            everything lost            RESURRECTED from makimono
the shell exits            session ends               session ends  (legitimate)
a human kills it           session ends               session ends  (legitimate)
```

## 3. The pieces

### 3.1 Views detach, they do not own

mado stops owning sessions. A window is a **view** onto a daemon session (the
Model↔View axis of SESSION-TYPESCAPE §1, finally honoured at runtime):

* `tear.sessions.detach_on_close = true` — closing a window or quitting mado
  detaches; the SIGTERM/SIGINT reaper detaches instead of `kill_session`.
* The runtime that makes this possible is the **daemon** runtime. The embedded
  runtime keeps existing (★★ modularize, don't delete) and keeps its honest
  semantics — its sessions die with mado — so selecting detachable sessions
  selects the daemon runtime; the two cannot be combined (typed config, §5).
* Session switching and the Ctrl-S picker work over `&dyn MultiplexerControl`,
  not over `InProcess`, so they behave identically on both runtimes and the
  MCP tools, the picker and the operator see ONE set of sessions.

### 3.2 tamotsu (保つ, "to keep") — the PTY holder

One tiny process per pane that owns the PTY master fd and the child:

* spawned by the daemon, double-forked + `setsid`, so it is nobody's child and
  outside the daemon's process group;
* serves ONE unix socket: `attach` (replay from offset N, then live bytes),
  `write`, `resize`, `signal`, `end(cause)`, `status` (pid, cwd, exit);
* writes every output byte to that pane's **makimono** journal itself, so no
  byte is lost while no daemon is attached;
* never decides that a session is over except when the child exits
  (`Ending::Exited`) or it is told `end(Human)`.

The daemon, on start, scans the session directory, connects to every live
holder socket and **re-adopts** the pane: it rebuilds the pane grid by
replaying the journal, then follows live bytes. `PtyHandle` gains a second
arm — `Held` — beside today's in-process one; everything above `PtyHandle`
(`PaneGrid`, subscribers, snapshots) is unchanged.

Kept deliberately small and slow-moving: an upgrade of tear must not require
restarting the holders, so the holder↔daemon protocol is versioned
(`Request::Hello`-style capability probe, as `tear-client` already does).

### 3.3 makimono (巻物, "the scroll") — the durable pane journal

```
$XDG_STATE_HOME/tear/sessions/<guid>/
  session.json            SessionDefinition + layout + name, atomic (tmp+fsync+rename)
  panes/<slot>/
    meta.json             spawn spec, last cwd, last title, holder pid, geometry
    seg-000041.raw        raw PTY output, append-only
    seg-000042.raw
    ending.json           present ONLY after a legitimate end (the tombstone)
```

* **Raw bytes, not rendered cells.** Replaying the byte stream through the one
  VT parser reproduces the grid exactly (colours, alt-screen, marks) and keeps
  a single source of truth — no second serialisation of `PaneGrid` to drift.
* **Bounded:** segments of 4 MiB, oldest evicted past `max_bytes` (default
  64 MiB per pane) — the unbounded-scrollback incident (90 GB) is the reason
  the bound is not optional.
* **fsync cadence:** every 1 s or 1 MiB, whichever first. A crash loses at most
  that window of *display history*; it never loses the session.
* **cwd:** tracked from OSC 7 in the stream AND polled from the child
  (`/proc/<pid>/cwd`, `proc_pidinfo` on macOS) into `meta.json`, so a shell
  that never emits OSC 7 still resurrects in the right directory.

Why files and not SQLite: the workload is one append-only byte stream per pane
plus a tiny metadata document. Append-only segments are the cheapest correct
shape for that (no write amplification, trivially bounded by deleting a file,
replay is a sequential read); a database would buy transactions the journal
does not need.

### 3.4 The typed ending

```rust
/// The ONLY two ways a session legitimately ends.
pub enum Ending {
    Exited { code: Option<i32> },
    EndedBy { who: Human, at_unix: u64 },
}
```

`ending.json` (the tombstone) is written only from a value of this type. On
start the daemon classifies every session directory:

| holder socket answers | tombstone | verdict |
|---|---|---|
| yes | — | **Held** → re-adopt |
| no | present | **Ended** → archive, show nothing |
| no | absent | **Orphaned** → **resurrect** |

"Orphaned but not resurrected" has no arm.

### 3.5 Resurrection is a new incarnation

A resurrected session is a fresh `InstanceId` of the same `DefinitionId`
(SESSION-TYPESCAPE's `reinstantiate`, which already exists), carrying
`resurrected_from` and the replayed journal above a separator line. The
processes that died are NOT claimed to have survived — `Durability::Held`
names survival of a daemon restart only, and nothing names survival of a
reboot. Illegal state #6 is therefore narrowed, not deleted: *"these processes
survived a reboot"* stays unrepresentable.

### 3.6 Ctrl-S — the one session surface

The existing picker (`mado/src/session_picker.rs`) grows, rather than a second
surface being built:

* every daemon session, with a state glyph: attached-here · attached-elsewhere
  · background · resurrected;
* a **preview** of the highlighted session (`pane_snapshot`, already on the
  wire) before attaching;
* actions: attach (Enter), send current to background (detach), end session
  (a human end — asks once, writes `Ending::EndedBy`), rename (exists);
* resurrected sessions are marked so the operator knows the process is new.

## 4. Config

```yaml
# ~/.config/tear/tear.yaml — read once, at daemon start
sessions:
  durability: held            # process_bound (default) | held
  journal:
    max_bytes_per_pane: 67108864
    segment_bytes: 4194304
    fsync_interval_ms: 1000
  store_dir: null             # default $XDG_STATE_HOME/tear/makimono
  holder_program: null        # default: this tear binary, `tear hold`
```

```yaml
# ~/.config/mado/mado.yaml — the view side
tear:
  runtime: resident           # embedded (default) | daemon | resident
```

The two halves are separate on purpose. `runtime: resident` is what makes a
window a view (closing it detaches); `durability: held` is what makes the
daemon's sessions outlive the daemon. Each default leaves today's behaviour
untouched, and the fleet turns both on together through the
`pleme.terminal` nix surface, which also derives the launchd
`AbandonProcessGroup` / systemd `KillMode=process` posture from
`durability: held`.

## 5. Invariants and their tiers

| Invariant | Tier |
|---|---|
| a tombstone is written only from an `Ending` value | truly-unrep (sole writer takes `Ending`) |
| `detach_on_close` with `runtime: embedded` | parse-time-rejected (config validation) — embedded sessions cannot outlive mado |
| an involuntary end finishes a session | truly-unrep at the classification table (no arm) |
| a resurrected instance claims the old processes | truly-unrep (distinct instance, no survival arm) |
| journal exceeds its bound | only-mitigated (eviction is runtime); CI red-run proves eviction fires |
| holders survive a daemon restart under launchd/systemd | CI/eval-caught (module assertion on the derived unit fields) + live receipt |

## 6. Phases — each ends on a verified receipt

| Phase | Delivers | Status | Receipt |
|---|---|---|---|
| **P1 views detach** | mado `tear.runtime: resident`; close/SIGTERM detach; Ctrl-S over `MultiplexerControl` | **Shipped** (mado, before 2026-10-06) | `TearRuntime::Resident` + its picker bridge in `mado/src/gui_tear_attach.rs` |
| **P2 makimono** | journal crate; resurrection on daemon start | **Shipped** | `makimono` unit tests (journal reopen/eviction/gap, first-ending-wins, archive retention); `tear/tests/held_sessions.rs::a_lost_holder_is_resurrected_in_place_on_the_next_start` — holder SIGKILLed while no daemon runs (the reboot shape), next start shows the old scrollback, the banner, and a different shell pid |
| **P3 tamotsu** | the holder; `PaneIo::Held`; re-adoption | **Shipped** | `tear/tests/held_sessions.rs::a_daemon_restart_reattaches_the_same_shell_with_its_screen` — same session id, screen rebuilt from the journal, `$$` identical before and after the restart; `tamotsu/tests/holder.rs` — detach/adopt keeps the pid, an exit code becomes the ending, a killed holder is revived in place |
| **P3b clients reconnect** | `tear-client` re-dials a restarted daemon; mado re-subscribes the displayed pane | **Shipped** | `tear-client::tests::a_client_outlives_a_daemon_restart_on_the_same_socket`; mado's resident loop re-attaches when its byte stream ends while the pane is still `Running` |
| **P4 fleet** | nix surface, derived launchd/systemd posture, rebuild | ships with the nix commit | rebuild, `launchctl kickstart -k` the daemon, sessions and their shells survive |

## 7. As built — where the build departed from the design

- **P2 and P3 merged.** The journal writer lives in the holder from the
  first commit rather than moving there in P3: every byte is written by the
  one process that is guaranteed to see it, so there was never a version in
  which the daemon journals and a later one in which it does not.
- **One durability enum, not three booleans.** §4's draft had
  `detach_on_close`, `auto_revive` and `persist.enable`. Held panes without
  a journal, or a journal without re-adoption, have no meaning, so the
  daemon side is a single `durability: process_bound | held` and the view
  side is mado's existing `runtime` enum.
- **Read once, at start.** `sessions` is not hot-reloaded. A mado built
  against an older `tear-config` round-trips `SetConfig` without the field,
  which would otherwise switch durability off on a live daemon.
- **Holder sockets live beside the daemon socket** (`<socket dir>/h/<pane>.sock`),
  not under the session directory: macOS caps `sun_path` at 104 bytes.
- **The holder is `tear hold`**, the same binary as the daemon, so no
  second package has to be installed or kept in step. The `tamotsu` binary
  exists for that crate's own tests.
- **Only the `tear daemon` binary may hold sessions.** `start_with_config`
  (the library entry every embedded and test daemon uses) never enables
  durability; `tear daemon` does, passing its own exe as the holder program.
  The first cut enabled it inside the library from the operator's
  `tear.yaml`, so once a workstation set `durability: held`, every test that
  started an in-process daemon (mado's suite, `DaemonHarness`) wrote into the
  operator's real store and spawned its test binary as `tear hold`
  (receipt: 10 junk sessions, `resurrections: 17`, holder logs reading
  `Unrecognized option: 'pane-dir'`). Sealed by
  `held_sessions::a_library_started_daemon_never_turns_durable_from_ambient_config`.
- **A daemon stopping hands its holders off explicitly.**
  `DaemonHandle::stop` calls `InProcess::release_durable`, which detaches
  every held pane and parks the persister. Without it an in-process daemon
  that stops while another `Arc<InProcess>` lives (the kanshou sidecar holds
  one) would keep repairing holders a successor daemon owns.
- **cwd.** OSC 7 from the grid is persisted by the daemon; the holder also
  polls `/proc/<pid>/cwd` on Linux. macOS has no unsafe-free poll yet, so a
  macOS shell that never emits OSC 7 resurrects in its spawn directory.
