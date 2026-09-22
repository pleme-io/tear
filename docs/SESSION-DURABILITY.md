# Session durability — a session ends only when it ends

> **Status: design, 2026-09-22. Nothing below is shipped unless its row in
> §6 says so.** This amends [`SESSION-TYPESCAPE.md`](./SESSION-TYPESCAPE.md)
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
tear:
  runtime: daemon            # required by detach_on_close (typed, §5)
  sessions:
    detach_on_close: true
    auto_revive: true        # re-adopt held panes; resurrect orphaned ones
    persist:
      enable: true
      max_bytes_per_pane: 64MiB
      fsync_interval_ms: 1000
```

Rendered by the nix module trio for mado and tear; the daemon unit gets
`AbandonProcessGroup` (launchd) / `KillMode=process` (systemd) whenever
`auto_revive` is on, derived rather than hand-set.

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

| Phase | Delivers | Receipt |
|---|---|---|
| **P1 views detach** | daemon runtime gets switching + the Ctrl-S picker over `MultiplexerControl`; close/SIGTERM detach; picker lists all daemon sessions with state + preview + end | kill mado with sessions open; relaunch; Ctrl-S shows them; attach restores the screen |
| **P2 makimono** | the journal crate; the daemon writes it; resurrection on daemon start | `kill -9` the daemon, reboot-equivalent (kill holders+daemon), relaunch: sessions come back in the right cwd with their scrollback |
| **P3 tamotsu** | the holder; `PtyHandle::Held`; re-adoption; journal writer moves into the holder | `kill -9` the daemon mid-`top`; relaunch; the same `top` pid is still running in the reattached pane |
| **P4 fleet** | nix options, launchd/systemd derivations, docs, SESSION-TYPESCAPE rows updated | rebuild cid; restart the daemon unit; sessions survive |
