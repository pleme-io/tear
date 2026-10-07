# Performance — pay for what changed, never for what came before

> **Status: DESIGN, destination-first. Designed 2026-10-07; nothing here is
> shipped.** It schedules and measures what is left of
> [`SHUKEN.md`](./SHUKEN.md) and builds on
> [`SESSION-DURABILITY.md`](./SESSION-DURABILITY.md), restating neither. It
> proposes three amendments to them, each named where it lands: SHUKEN §3's
> in-process carrier becomes an owned frame published by reference instead of
> a borrowed view (R26, §10); SESSION-DURABILITY §3.2's holder becomes
> upgradable in place and serves several sinks (R16, R21); and §3.3's
> re-adoption reads a checkpoint plus the journal tail (R27). The two
> SESSION-DURABILITY amendments are held for the operator (§8.2). §5 is the
> ladder: every rung says what it extends, what it should buy (with a
> receipt), the gate that proves it, whether it is an interim step on the path
> or the destination, and the configuration value that keeps today's behaviour
> reachable when that behaviour is a good state. §8 grades every invariant as
> a **TARGET** in selo's vocabulary: a row is re-graded only on a red run
> against a deliberately broken input, and §6 re-executes every red run on
> every CI run. Gate 0 for this seam is
> [`theory/MADO-TEAR-SEAM.md`](https://github.com/pleme-io/theory/blob/main/MADO-TEAR-SEAM.md).
> mado's
> [`GRID-THREADING-CONTRACT.md`](https://github.com/pleme-io/mado/blob/main/docs/GRID-THREADING-CONTRACT.md)
> points here for the tear ↔ mado path.

> **Receipts.** Every number carries one, inline. **L** passive profile of the
> live resident + held configuration (567 s) · **H** isolated harness driving
> the shipped 0.1.32 binary, cases a–f (§11) · **F** floor suite of OS
> primitives: a UDS, b PTY, c flush, d wake, e shared memory, f serialization,
> gpu, timers · **D** durability probes on the real `tear hold` · **M**
> multi-attach and observer probes · **G** benches of a byte-identical
> `PaneGrid` · **W** wire probes with the real `tear-client` · **C**
> view-codec prototype (two runs) · **P** protocol-compatibility and mutation
> probes · **R** prior art, named inline · **S** source at the pinned
> revisions (§11). A composite floor is a sum of separately measured hops,
> never an end-to-end run. The sources and raw results behind H, F, D, G, W,
> C, P and M are not in this repository yet: R1 ports them into `tear-bench`,
> scrubbed of paths, process ids and user names, and re-measures every number
> there. Until then each figure is that pass's receipt, not a gate. L is
> summarized only, because it profiles a private machine.

## 0. The destination

Between a program writing to its PTY and a photon on the operator's panel,
every hop does work proportional to **what changed**, never to **how much
happened before**, and nothing waits on anything it does not need.

1. **One parser, one coordinate.** A pane's bytes are parsed once, by the
   authority (SHUKEN). Every byte, frame, event and cursor carries the pane's
   absolute output offset, so attach, resume and restart are exact.
2. **Delivery never waits on durability or on a viewer.** The holder forwards
   first and flushes beside. Only the authority may slow the child; a slow
   viewer costs one retained frame, and a slow byte consumer reads the
   journal.
3. **State is conflated, events are not, bytes are addressed.** A viewer
   receives the difference between the last frame it was sent and the newest;
   events travel an append-only ring; byte consumers read by offset.
4. **Nothing polls.** Output wakes the window, a pane's end is an edge, and an
   idle system makes no wakeups.
5. **The UI thread never blocks on tear.** It enqueues intents, draws at most
   one frame per display refresh from the newest view, and sleeps.
6. **The session tree runs in the band of the work it hosts.**
7. **Every query has one answer.** Whoever holds a pane's answering lease
   answers its terminal queries — the authority when nobody does — so a query
   is never answered twice or left unanswered.
8. **Every claim is a gated cell.** Thirteen cases × every metric, each a
   multiple of a floor measured in the same run or a count, each with a red
   run proving the gate sees its own regression.

| Case | Destination | Rungs |
|---|---|---|
| C1 embedded | one parse per byte; the renderer loads the newest frame by reference, and attach is an `Arc` load | R21, R26, R38 |
| C2 daemon | a key costs two socket hops and a PTY echo at any history depth; bytes cross the wire as bytes | R2, R5, R6, R8 |
| C3 resident | an idle window makes no RPCs, no key is lost at any depth, and a pane no window shows still answers its queries | R3, R5, R10, R42 |
| C4 held | durability flushes beside delivery, never in front of it, and a stalled consumer loses nothing | R4, R17, R21 |
| C5 multi-attach | N windows cost N subscriptions and N viewports, one parse, one answer per query and one retained frame per stalled window | R22, R34, R38, R42 |
| C6 remote | a typed key costs tens of bytes on any carrier, clocked by acks and resumable | R34, R39 |
| C7 attach, switch, reattach | a swap of ≤1 ms of UI time whose keyframe is sized by the screen's content, never by its history | R5, R9, R27, R32 |
| C8 startup | a daemon accepts within 3× spawn + `openpty` of its start whatever the pane count, and a window's first frame arrives at max(GPU init, tear setup) | R27, R32 |
| C9 flood | a flood reaches the screen at ≥0.6× the PTY ceiling with no per-KiB allocation, and history costs about the text it holds | R21–R26 |
| C10 idle | a daemon, a holder and a bridge make no timer wakeups; a window costs what a parked window costs | R10, R11, R18, R39 |
| C11 resize | a drag costs one surface reconfigure per frame and, per settled size, one reflow at the authority and ≤2 SIGWINCH | R23, R33 |
| C12 input | a key costs the UI one enqueue and one notify; a paste of any size blocks nothing and arrives whole | R5, R13, R23, R35 |
| C13 observers | an observer read costs ≤2× one RPC and O(screen) at any depth, never touches the UI thread, and never reports blind as found | R9, R36, R40 |

On the reference Mac that means: a key costs the time of a plain echo at any
history depth, and costs the UI thread one enqueue and one notify (~1.2–2.3
µs, F-e sender side) while the writer wakes ~6 µs later (F-d); attach and
switch cost ≤1 ms of UI time and a keyframe sized by the screen's content, not
its history (63 B–6.4 KB for text, 87 KB for per-cell truecolor, C); a flood
reaches the screen at ≥0.6× the PTY ceiling; an idle daemon, holder and bridge
make 0 RPCs and ≤1 wakeup a second, and an idle window costs no more than a
parked window with no tear link; history costs about the text it holds; a
typed key over a remote link costs ~38 B (C, input frame estimated); and every
query gets one answer whoever is watching.

## 1. What is a world-fact, and what is ours

### World facts — typed as limits

| World fact (receipt) | What it forces |
|---|---|
| A macOS PTY master read returns ≤1,024 B whatever the buffer: p50 = max = 1,024 B over 65,536 reads (F-b); Ghostty's and kitty's sources say the same (R) | coalescing belongs above the read, in a gather stage; a bigger buffer buys nothing |
| A raw-mode PTY takes ~1,022 B of input before a write blocks, while a cooked-mode one accepted 64 MiB without pushback (G; XNU `TTYHOG`); raw-mode paste drains at 27.4 MB/s, 5.9 MB/s in the background band (F-b) | input is a per-pane queue drained by a writer; a 16 MiB paste into a raw-mode child takes ≥0.6 s at the kernel, whoever waits for it |
| PTY output peaks at 224.9 MB/s in the interactive bands and 70.7 MB/s in the background band (F-b) | the denominator of every throughput budget |
| A UDS round trip is 4.13 µs with one write per frame and 7.38 µs with a separate length write (64 B, F-a); AF_UNIX buffers default to 8 KiB (`net.local.stream.sendspace`, F) | small messages are bound by thread wakes; the buffer size is a default, ours to raise |
| A cross-thread wake costs the woken side 5.75–6.46 µs hot and 12.9–48.8 µs from idle, p99 712 µs through the run loop (F-d); the waking side pays 1.2–2.3 µs (F-e) | an event-driven wake costs under 1 % of a 120 Hz frame at p50; polling buys nothing |
| A futex-woken shared-memory ring is no faster than UDS for one message: 7.50–7.54 vs 7.08 µs p50 (F-e) | shared memory buys batching, not latency |
| `F_FULLFSYNC` p50 3.73–4.20 ms (F-c) and 4.45–5.92 ms (D; 5.9 ms after a 1.5 s idle); `F_BARRIERFSYNC` 0.22 ms (F-c) and 0.6–1.41 ms (D); macOS `fsync(2)` 26–92 µs (F-c, D) and does not flush the drive, where Linux `fdatasync` does (`man 2 fsync`, `man 2 fdatasync`); Rust's `sync_data` is `F_FULLFSYNC` on Apple (S std). The two probes disagree; within either, a full flush costs 3–17× a barrier | *where* a flush runs is ours; what each primitive promises is the world's (`man 2 fcntl`), and it differs by OS |
| The background band (PRI 4) turns `sleep(1 ms)` into 19.7 ms and multiplies wakes 5–9× at p50 and ~100× at p99 (F-timers, F-d); a CPU loop runs 8.3× slower (R, probed on the reference Mac); a child inherits its parent's band at spawn — the live holder's threads read 4/4/63 and its shell's 4/4/4 (F) | the band is the world's; being in it is ours, and a running process keeps the band it was born in |
| launchd's `Adaptive` leaves the background band only on XPC activity (`launchd.plist(5)`) | tear speaks UDS, so for tear `Adaptive` means `Background` |
| A composited window waits half a refresh on average, 4.2 ms at 120 Hz; keyboards add 8–60 ms (R: Fatin, Dan Luu) | the app's budget sits under these; nothing here chases below them |
| Wayland has no occlusion event — a hidden surface simply receives no frame callbacks — and winit reports none there (S winit 0.30.13 `event.rs:421`) | Hidden is inferred from frame callbacks on Wayland |
| `CADisplayLink` needs macOS 14; `CVDisplayLink` is deprecated in 15; mado ships for 11.0 (R, S) | the vsync source is chosen at run time |
| A program that sends a terminal query waits out its own timeout when nothing answers: crossterm waits 2 s for a cursor report (R: crossterm 0.28.1 `cursor/sys/unix.rs:39`) | every query needs exactly one answerer, whether or not a window shows the pane |
| Browsers parse 5–35 MB/s and WebSockets have no receive backpressure (R: xterm.js); Linux delays ACKs 40–200 ms (R) | mado-web needs application acks; every TCP socket needs `TCP_NODELAY` and one write per frame |

### Ours — dissolved here

| How it presents | Why it is ours | Dissolved at |
|---|---|---|
| typing slows as a pane grows: 3.41 ms a key (H-b), its DECCKM snapshot RPC alone 4.5 ms at 0 rows and 91.4 ms at 1,453 (H cliff), keys lost past ~1,667 rows (H) | DECCKM is read through a full-history snapshot, and the client reuses a desynchronized stream | R3, R5 |
| the daemon is slow: a held flood at 19.3 MiB/s, `NewSession` 218.6 ms (H, daemon and holder at PRI 4) | our launchd default, `Adaptive` | R2 |
| echoes spike once a second (H-c) | our ordering: flush before forward | R17 |
| output trails the shell by up to 16.7 ms (S) | our pacing: `Capped(60)` and no wake path | R10, R11 |
| attach fails on long panes | our encoding: 60 B per cell over unbounded history, under a 16 MiB frame cap (H) | R9, R34, R37 |
| floods crawl: 54–65 MiB/s under a 213.6 MiB/s ceiling (H-d) | our pump: 1 KiB frames, per-chunk allocation, fan-out under a global lock — embedded ≈ daemon, so not IPC (H-d) | R21–R25 |
| memory explodes: 2.2 GiB per 64 MiB printed (H-d) | our rows: 2,639 B per scrollback row at 163 columns (G) | R26 |
| two views of one pane disagree | our layout: two parsers | R30, R38 |
| a query in a pane no window shows goes unanswered, and one in a pane two windows show is answered twice (S) | our layout: the answerer is whichever mado displays the pane, and the authority's answers are never drained | R42, R43 |
| the compositor is told the whole surface changed on every present | wgpu's `present()` takes no damage (S mado `grid_damage.rs:30-37`); on a Linux compositor that is a full-surface copy per present (S, measured on a Linux seat) | mado's own share at R31; handing damage to the compositor needs a surface API that takes it (§8.3) |

## 2. The measured baseline

Cases: C1 embedded · C2 daemon, window-owned · C3 resident · C4 held
durability · C5 multi-attach · C6 remote (TCP, ssh, ws-bridge → mado-web) · C7
attach, switch, reattach · C8 startup · C9 flood and throughput · C10 idle ·
C11 resize and reflow · C12 input (keys, paste, mouse) · C13 observers and
control traffic (MCP, picker, praça, audit).

Today and the floor are measured on the reference Mac (§11). A target is a
multiple of a floor measured in the same run, or a count; where the table
states an absolute value it is the reference-Mac acceptance, and the matrix
encodes it as a multiple of that run's floor (§6), so it holds on any host
class. Echo floors are sums of separately measured hops. A *warm* hop is a
one-way send to a receiver woken hot, 7.08 µs (F-e); an *idle* hop is the
sender's write, 1.17 µs (F-e sender side), plus a wake from idle, 12.9 µs
(F-d); each echo adds one PTY echo, 5.17 µs, measured hot (F-b). A cell
today's code already meets in some recorded run guards against regression and
is not a rung's evidence (§6).

| Case | Metric | Today | Floor | Target | Rungs |
|---|---|---|---|---|---|
| C1 embedded | echo, warm (no gap), p50 | 18.9 µs (H-b, n=1,000) | ~11 µs: PTY echo + one hot wake, 5.79 µs (F-b, F-d) | ≤2× (met: no regression) | R21, R38 |
| C1 | echo after a 2 ms gap, p50 / p99 | 72.8 / 355 µs (H-b, n=1,500) | ~18 µs: PTY echo + one wake from idle | report-only: its run-to-run spread is 2.39× (§6) | — |
| C1 | VT parses per output byte | 2 (S `inproc.rs:631`) | 1 | 1 | R38 |
| C1 | 64 MiB to a consumer | 65.5 MiB/s (H-d) | 213.6 MiB/s PTY ceiling (H-d) | ≥0.6× | R21–R25 |
| C1 | attach after 0.25 → 8 MiB of history | 1.3 → 39.5 ms (H-f) | an `Arc` load; a publish costs 0.62–2.88 µs at 163×48 (C) | flat in history | R26, R38 |
| C2 daemon | `get_pane` round trip, p50 / p99 | 12.0 / 21.7 µs at PRI 31 (H-a, n=3,000); 24.96 µs p50 framed with the server at PRI 4 (F-a) | 7.38 / 11.88 µs framed UDS (F-a) | ≤2× / ≤3× (met at PRI 31; red in the deployed band) | R2 |
| C2 | echo, warm, p50 | 23.3 µs at PRI 31; 58.1 µs with the daemon at PRI 4 (H-b) | ~19 µs: 2 warm hops + PTY echo | ≤2× | R2 |
| C2 | echo after a 2 ms gap, p50 | 156.0 µs (H-b) | ~33 µs: 2 idle hops + PTY echo | report-only (§6) | — |
| C2 | wire bytes per output byte under a 64 MiB flood | 1.98 (H, F-f) | 1.00 | ≤1.02 after R6 (1,038 B per 1 KiB frame, F-f); ≤1.001 after R21 | R6, R21 |
| C3 resident | idle `get_pane` RPCs | 54–61/s, ~90 % of the daemon's CPU (L) | 0 | 0 | R10 |
| C3 | keys delivered in a 3,000-row pane | 0 of 20 (H) | 20 of 20 | 20 of 20 | R3, R5 |
| C3 | answers to a query in a pane no window shows | 0: the asker waits out its timeout, 2 s for a crossterm cursor report (S tear `pane_grid.rs:1615, 1625`; R crossterm) | 1 | 1 | R42 |
| C4 held | echo, warm, p50 | 32.5 / 32.1 µs at PRI 31 (two runs); 108.7–136.0 µs in the deployed band (H-b) | ~33.5 µs: 4 warm hops + PTY echo | ≤2× | R2 |
| C4 | echo after a 2 ms gap, p50 | 118.7–283.9 µs at PRI 31; 243.9–267.4 µs in the deployed band (H-b, two runs) | ~61 µs: 4 idle hops + PTY echo | report-only (§6) | — |
| C4 | echoes >3 ms in a 15 s typing series | 24 and 19 of 1,500, 3.0–13.8 ms; 10 of 23 and 12 of 18 inter-spike gaps at 990–1,040 ms (H-c, two runs) | 1–2 in the flush-free variants (H-c, one run each) | ≤ max(4, 2× the same run's `page_cache` series) | R17 |
| C4 | first echo after a 1.5 s pause, holder hop only | p50 7.6 / p90 25 / max 38 ms (D, n=15) | 0.24 ms with flushing off (D) | ≤0.5 ms on the holder hop | R17 |
| C4 | first echo after a 1.5 s pause, full chain | unmeasured | the same run's `page_cache` variant | ≤1.5× that | R17 |
| C4 | 64 MiB flood | 54.0 MiB/s; 19.3 with the daemon and holder at PRI 4 (H-d) | 213.6 MiB/s (H-d) | ≥0.6× | R2, R17, R21 |
| C4 | output after a >2 s consumer stall | muted until restart: 7,259 B in 3 s while the journal grew 2.88 MB (D) | — | 0 B lost | R4, R21 |
| C5 multi-attach | daemon CPU per 128 MiB at 0 / 1 / 2 subscribers | 1.30 / 2.51 / 3.61 s (M) | 1.30 s | +≤0.05 s per extra | R22, R34 |
| C5 | daemon memory behind one stalled viewer | 1.6 → 280.7 MB (M) | one frame, ~125 KB (7,824 cells × 16 B) | one frame (view); ≤4 MiB (a held pane's bytes); ≤16 MiB (a process-bound pane's bytes) | R22, R34 |
| C5 | VT parses with N windows | 1 + N (S) | 1 | 1 | R38 |
| C5 | answers per query with N windows | N (S mado `gui_tear_attach.rs:677-695`) | 1 | 1 | R42 |
| C6 remote | TCP connect / subscribe, p50 | 29.6 / 53.6 ms (H, n=30) | the UDS path's 0.29 / 0.25 ms (H) plus one TCP round trip, 14.5 µs (F-a) | <1 / <2 ms | R8 |
| C6 | bytes per typed key | 469,852 B: 36 B `SendKeys` + 469,816 B snapshot (H) | — | ~28 B delta + ~10 B input, compressed (C) | R5, R34, R39 |
| C6 | remote GUI attach | none: `--tcp` is a separate, non-durable daemon (S `main.rs:1896-1904`) | — | lanes over TCP, ssh, WebSocket | R39 |
| C7 attach | mado-shaped attach to first live key at 0.25 / 1 / 4 / 8 MiB | 154–162 / 582–607 / 2,299–2,361 ms / dead (H-f, n=3) | keyframe 3.3–6.4 KB for text and 87 KB for per-cell truecolor, 24.7 µs encode + decode for a build log (C) | ≤1 ms of UI time, flat in history | R9, R32 |
| C7 | history replays per daemon attach | 2: engate's snapshot RPC and the daemon's first frame, the second through the answering path (S daemon `lib.rs:1002-1016`; Gate 0 class 8) | 1 | 1 | R5 |
| C7 | duplicated output on attach mid-flood | >0 (S daemon `lib.rs:964-996`) | 0 | 0 | R5, R20 |
| C7 | modes after a replay | defaults (S `pane_snapshot.rs:372-480`) | the authority's `ModeSet` | equal | R5 |
| C7 | re-adoption with an 8 MiB journal | ready 14.0–17.4 ms, live 98–100 ms after the restart (H, n=3) | checkpoint + tail | each pane ≤1.25× its tail's replay | R27 |
| C8 startup | daemon start → socket answers | 8.6 / 22.6 / 38.7 ms bound / held / held with the daemon at PRI 4 (H, n=5) | spawn + `openpty`, measured from R1 | ≤3× | R2, R27 |
| C8 | `NewSession` | 2.06 bound, 23.8 held, 218.6 held with the daemon at PRI 4, ms (H, n=25) | 2.06 ms (H) | held ≤5× bound (≤10 ms on the reference Mac) | R2, R17, R27 |
| C8 | restart → accepting | 3–6 ms at earlier logged restarts; 540–547 ms at the last two, while the file watcher was built before the accept thread (S, daemon log); 14.0–17.4 ms in isolation with an 8 MiB journal (H, n=3) | spawn + `openpty` (R1) | ≤3× at every start | R27 |
| C9 flood | frames per MiB under a saturating flood | 1,024: 65,537 frames of 1,024 B per 64 MiB (H-d) | 16 at 64 KiB | ≤32 | R21 |
| C9 | short lines (`yes`, 163 columns) | 9.6 MB/s with 2-byte lines and 513 allocations per KiB (G); 13.9 MiB/s with the 3-byte lines a PTY delivers (measured in the daemon code read, §11) | vte with a no-op performer, 1,875–1,961 MB/s (G) | 0 allocations per KiB; throughput `Pending` until R24's first run backs a multiple | R24, R25 |
| C9 | daemon RSS per 64 MiB flood | 20 MiB → 2.2 GiB, kept after the kill (H-d, n=3) | the text itself | ~1–1.5× the text, released | R26 |
| C10 idle | GUI loop ticks | 57.83/s (L, n=32,801 over 567 s) | 0 | ≤1/s | R10, R11 |
| C10 | context switches/s: GUI / daemon / holder | 215 / 77 / 2.8 (L) | GUI: a parked madori window with no tear link, measured from R1; daemon and holder ~0 | GUI ≤1.5× that floor; daemon and holder ≤1 each | R10, R11, R18 |
| C10 | painted frames repeating content | 74.1 %: 13,793 of 18,610 (L) | 0 | ≤5 % | R12 |
| C10 | WebSocket bridge timer wakeups | 25/s: a 50 ms accept poll and a 200 ms main poll (S) | 0 | 0 | R39 |
| C11 resize | per drag step | an O(scrollback) mirror rewrap + a blocking RPC + a truncating resize + SIGWINCH (S); timings unmeasured | — | ≤1 resize per 50 ms; 1 reflow and ≤2 SIGWINCH per drag; ≤1 ms of UI per step | R23, R33 |
| C12 input | mado-shaped key, p50 | 3.41 ms at PRI 31 and 7.6–8.6 ms with the daemon and holder at PRI 4 (H-b, n=1,000); its DECCKM snapshot RPC alone grows from 4.5 ms at 0 rows to 91.4 ms at 1,453 (H cliff, n=10 per depth); keys lost past ~1,667 rows (H) | a plain key's echo, 0.12–0.16 ms (H-b) | ≤1.25× plain (R5); ~1.2–2.3 µs of UI per key (R13) | R2, R5, R13 |
| C12 | paste | an 8 MiB paste blocks the UI ~0.3 s while the child drains it (27.4 MB/s, F-b); anything above ~8.1 MiB is lost (W) | 27.4 MB/s kernel floor, raw mode (F-b) | 0 ms of UI at any size, ≥0.8× the floor | R3, R13, R35 |
| C12 | hovering a mouse-tracking TUI | 60–120 blocking `SendKeys`/s plus up to ~240 `GetPane`/s at an assumed 60–120 Hz pointer rate (S mado `ux/modes.rs:1033`, `ux/engine.rs:2331-2333`, `gui_tear_attach.rs:1154-1162`; rate estimated) | 1 report per frame | ≤1 per frame, non-blocking | R13, R35 |
| C13 observers | MCP `pane_snapshot_text` | 61 ms at 1,000 rows (M; the RPC under it takes 61.7 ms at 953 rows, H); fails past ~1,667 rows at 163 columns and at 1,000 rows at 300 columns (H, M) | ~4 KB keyframe, ~25 µs (C) | ≤1 ms at any depth | R9, R40 |
| C13 | per MCP call | connect + Hello + N+1 RPCs; `daemon_status` 3.3 ms against `list_panes` 0.2 ms (M) | one RPC | ≤2× an RPC | R40 |
| C13 | Ctrl-S picker keystroke | one blocking `GetSession` per session (S `session_picker.rs:831`) | 0 | 0 on the UI thread | R13, R36 |
| C13 | audit emission | one `write` per lifecycle request when configured, none per key (S daemon `audit.rs:65-78`; L) | — | unchanged, counted | R1 |

## 3. Root-cause classes

The 2026-10-07 pass recorded 131 defects across seven code reads (§11), 117 of
them confirmed in code. Every one maps to a class below; §5 places each, and
§8.3 lists what it defers.

| # | Class | Mechanism (receipts) | Cases |
|---|---|---|---|
| 1 | The session tree runs in the background band | launchd `Adaptive`, substrate module-trio's default (S `module-trio.nix:499-511`), with no XPC: daemon, holders, shells and their children at PRI 4 (L, F), each inheriting the band at spawn (F); with the daemon and holder at PRI 4, held flood 54.0 → 19.3 MiB/s, held `NewSession` 23.8 → 218.6 ms, the mado-shaped key 3.46 → 7.6–8.6 ms; with the client there too, as when mado is App-Napped, 8.4 MiB/s and 18.2 ms (H); on Linux, held panes share the daemon's unit (S tear `flake.nix:57-63`) | C2–C4, C8–C10, C12 |
| 2 | O(history) on interactive paths | `PaneSnapshot` is the screen plus all scrollback at 60 B per cell, cloned under the grid lock (S); it is fetched on every key for DECCKM, two or three times per attach and on every MCP read; it crosses 16 MiB at ~1,667 rows (H); scrollback is unlimited by default (S `tear-config` `lib.rs:238`) | C2–C7, C12, C13 |
| 3 | Lockstep on the UI thread | every tear call is a blocking `rpc()` under one mutex on the AppKit thread, with no deadline (S `tear-client` `lib.rs:647-667`); the daemon serves one request at a time per connection (S); a paste blocks for the child's drain and is lost above ~8.1 MiB (W) | C2–C5, C12, C13 |
| 4 | Ticks where edges exist | no wake path (S `gui_tear_attach.rs:1372`) and `Capped(60)` (S `config.rs:2702`): one `get_pane` per idle tick (L); a 200 ms daemon loop, a 500 ms persister, a 250 ms holder tick, a 2 s cwd poll, a 50 ms TCP accept sleep, a 50 ms accept poll and a 200 ms main poll in the WebSocket bridge (S) | C1–C4, C6, C10, C12 |
| 5 | Durability in front of delivery | `F_FULLFSYNC` inline in `Journal::append` — on the interval and at every 4 MiB segment rotation — under the holder lock, before forwarding (S `journal.rs:128-158, 214-217`, `holder.rs:163-172`); session documents and pane metas flushed on request threads (S `durable.rs:176, 224, 244-254`), and the praça snapshot too (S daemon `praca_store.rs:140-153`) | C4, C8, C9, C12, C13 |
| 6 | Unbounded queues, no resync, silent loss | an unbounded queue per subscriber, filled under a global lock (S `inproc.rs:346, 636-648`); two more unbounded queues and a relay thread on mado's side (S tear-client `engate_producer.rs:62-69`, mado `stream_watch.rs:44-59`); a holder drops a failed sink and keeps its socket open (S `holder.rs:181-183`, D); an evicted journal offset is clamped without a word (S `journal.rs:169-171`) | C3–C6, C9, C13 |
| 7 | Per-KiB fixed costs | each 1 KiB read becomes a journal write, a frame, a send and a dispatch (F-b, H-d); 513 allocations per KiB of `yes` (G); an encode and a copy per subscriber (M); embedded matches the daemon's throughput, so the bound is not IPC (H-d) | C1–C4, C9 |
| 8 | Encoding waste | byte payloads as CBOR integer arrays, 1.98× the bytes and 60–173× the round trip (F-f); 60 B per cell (H); two writes per frame on the daemon's unbuffered streams and for client frames of 8 KiB or more, a header and a body read per frame, 8 KiB socket buffers, no `TCP_NODELAY`, a 50 ms accept sleep (S, F-a, H) | C2–C7, C9, C12 |
| 9 | Two models of one truth | every byte parsed twice, C1 included (S `inproc.rs:631`); the authority parses 5 OSC codes where mado parses 19 (S tear `pane_grid.rs:1368-1404`, mado `terminal.rs:6469-6487`); every query answered by each window that shows the pane and by nobody when none does (S mado `gui_tear_attach.rs:677-695`, tear `pane_grid.rs:1615`); a replay leaves the mirror's modes at their defaults (S); tear truncates on resize while mado reflows (S `pane_grid.rs:1729-1731`); a UTF-8 character split across reads loses bytes in `PaneGrid` (G) | C1–C5, C7, C11 |
| 10 | Painting what nobody asked for | DEC 2026 is decided after a drawable is acquired, so a deferral presents it unpainted (S `render.rs:6791-6808`); a 3-frame scrub per change, 74.1 % repeat paints (L); a full rebuild per frame, two last-frame readings of 886 and 3,634 µs of CPU through submit (L, n=2; acquire and GPU excluded); a new Metal buffer per write, 7.1 µs (F-gpu); ≥3 full-surface passes (S); occlusion ignored (S madori `app.rs:1394`) | C1–C3, C9–C13 |
| 11 | No fence, no identity | subscribe registers before it snapshots and calls the overlap harmless (S daemon `lib.rs:964-996`), and every attach replays history twice (Gate 0 class 8); `PaneBytes` carries no pane, offset or generation; subscriptions never authenticate (S); a re-dial drops the client's identity and keeps a stale capability view (S tear-client `lib.rs:316-326, 651-655`); two daemons on one store displace each other (SESSION-DURABILITY §7) | C3–C7, C13 |
| 12 | Unmeasured and ungated | no bench in tear; `ci.yml` disabled at the GitHub level since 2026-05-22 (S, Actions API); the `engate` feature's tests do not compile (P); the `Capability::ALL` pin passes with a variant missing (P); the fleet's benchmark action reports 0 regressions whatever happens (S actions `benchmark-runner/action.yml`) | all |

## 4. Architecture

### 4.1 The planes

```
child ─PTY─► gather ─batch{at}─► holder (held) or InProcess (local, embedded)
             1 KiB reads →          ├─► makimono journal ── a syncer flushes beside, never in front
             ≤64 KiB batches        └─► authority sink: bounded, the only thing that can pause the PTY
                                           │
                    PanePipe: feeder ─► PaneGrid (the one parser) ─► OwnedPaneView (RCU, held during DEC 2026)
                                       │    answers queries while no consumer holds the lease
                                       └─► events{at} (append-only ring)
                                           │
       LaneSet per consumer, every frame fenced by GridEpoch{incarnation, gen, at}
       session (RPCs, feed) · interactive (deltas + events ▼, input + acks ▲) · bulk (pages) · bytes{at}
                                           │     in process (C1): an Arc load and an enqueue
mado: LinkHandle (enqueue only) · replica · Viewport · madori scheduler · garasu row slots, one pass
```

### 4.2 The authority

- **One pipeline per pane, nothing global on the byte path.** A gather stage
  turns 1 KiB reads into batches stamped with their offset; the holder (held
  panes) or `InProcess` (local and embedded) forwards each batch to the
  authority first and appends it to the journal second; a syncer flushes
  beside, under a typed policy (R17).
- **Only the authority slows the child.** Its sink is the only one whose full
  queue pauses the PTY — `PauseReader`, the single backpressure arm mado's
  GRID-THREADING-CONTRACT already names. Observers read lagging ranges from
  the journal; a lagging view gets a fresh delta, never a backlog; a lagging
  byte consumer of a held pane reads the journal by offset, and one of a
  process-bound pane keeps a capped backlog and then resyncs in band (R22).
- **One parser publishes frames.** After each parse batch the authority
  publishes an immutable `OwnedPaneView` — SHUKEN's owned carrier, used in
  process too, because SHUKEN §3's borrowed `PaneView<'_>` would hold the
  parser's lock while a renderer reads — with `Arc` rows, the complete
  `ModeSet`, cursor, title, cwd, palette, interned style and link tables,
  graphics placements, damage and `fed_through`, through `ArcSwap`. Its
  `epoch: GridEpoch` grows to `{incarnation, gen, at}`. A publish costs
  0.62–2.88 µs at 163×48 and 4.79 µs at 212×58, independent of history by
  construction (C; depth not varied).
- **DEC 2026 is resolved here, once.** No frame is published inside a
  synchronized update until ESU or 100 ms / 2 MiB (§4.6), and the pipe waits
  on the hold's own deadline, so a BSU followed by silence still publishes at
  the bound. Parsing continues inside the block, so DSR and CPR are answered
  at once. The input modes — DECCKM, DECKPAM, kitty flags, mouse encoding,
  bracketed paste, focus — are published ahead of held cells, so a key never
  waits out a hold under stale modes. A keyframe requested mid-update is the
  last published frame.
- **Events are not state.** Bell, OSC 9/777/99 notifications, OSC 52 and the
  pane's end go to a bounded per-pane ring keyed by offset: never conflated,
  read through per-consumer cursors, overflow reported as a typed `EventGap`.
  A replay below the adoption boundary fires nothing; across a clean handoff
  an event is delivered exactly once, across a daemon crash at most once; side
  effects — an OS notification, a clipboard write — are deduplicated per
  process by (pane, offset).
- **History is bytes.** Sealed 256 KiB segments of UTF-8 text and style runs,
  read by range; reflow re-lays logical lines.
- **One answerer per pane.** The authority answers terminal queries for a pane
  unless a consumer that answers holds the pane's lease (every mado before the
  flip does). With no viewer, answers that describe a renderer — DA1's
  parameters, cell size, graphics and keyboard protocols — come from the last
  declaration or a configured default. A second interactive byte relay is
  refused for that attach alone.

### 4.3 The lanes

| Lane | Down (authority → consumer) | Up | Flow control |
|---|---|---|---|
| session | `DaemonHello` and capabilities, lane tickets, tagged responses, a registry feed with per-entity generations | `Hello{client_id, client_capabilities}`, `MintLanes`, tagged requests with deadlines | deadlines; the feed conflates per entity |
| interactive | view deltas and keyframe chunks, events (never conflated), pane end, input acks | serial-numbered input, streamed paste, viewport, theme and caps declarations, render acks | ≤2 unrendered deltas per (consumer, pane); paste under a 64 KiB credit; events at once |
| bulk | keyframes, history pages (≤1,000 rows), graphics by id, searches, exports | requests, cancel | windowed per consumer |
| bytes | `DATA{at}` frames, `Gap` then a VT keyframe | `OpenBytes{from, mode}` | a cursor over the journal (held) or a bounded ring |

Each lane is one full-duplex connection per flow-control domain, so input
going up and deltas coming down never share a kernel buffer and a stalled
reader of one class cannot block another; panes multiplex inside a lane under
per-pane credits. No lane frame exceeds 64 KiB; `MAX_FRAME_BYTES` stays only
as a decoder sanity bound. One coordinate fences every lane:
`GridEpoch{incarnation, gen, at}` — the authority process, the geometry
generation, and the pane's absolute output offset that makimono already
journals and tamotsu already frames. A keyframe is at N; deltas, bytes and
events follow from N; nothing is delivered twice. In process (C1) the same
lanes are handles: the renderer loads the newest `Arc<OwnedPaneView>` and
input enqueues into the pane's writer. Remotely (C6) they ride TCP,
ssh-forwarded sockets or WebSockets, clocked by acks and compressed only
off-host. The legacy wire (`PaneSnapshot`, `Subscribe`/`PaneBytes`, lockstep
`SendKeys`) stays served for old clients, made correct (§7).

### 4.4 The consumer

mado's UI thread does three things: it turns operator input into typed intents
that it enqueues and never awaits; it turns the newest view plus the window's
own `Viewport` (scroll offset, selection, search, URL detection, zoom, kinetic
scroll — the SHUKEN §5-B border) into at most one frame per display refresh;
and it sleeps. Its only handle to tear is a `LinkHandle` whose methods
enqueue; results come back through a mailbox and one coalescing doorbell. Keys
are encoded at the renderer with the input modes of the newest frame, which
the authority publishes ahead of any held cells; a key encoded under input
modes older than the authority's is refused `KeyModeSkew`, alone, and
re-encoded. Pastes are framed there too and declare the mode they were framed
under. Search, copy, URL detection in scrollback and prompt marks read the
replica, which keeps rows by stable line id and backfills through bulk pages.
madori schedules frames by demand — Parked, Hot or Hidden — against a display
link; garasu draws the grid in one pass from row slots keyed by (row id,
version). N windows on one pane cost N subscriptions and N viewports, never N
parses and never N answers.

### 4.5 Scheduling and placement

Scheduling is declared where the program is defined. substrate's module trio
renders launchd and systemd scheduling from a closed `workloadClass` keyed on
who waits on the process; tear declares `session-host`, so its daemon and
holders run where interactive work runs, and on Linux each holder runs in its
own scope so the daemon's weights are the daemon's alone. A band is inherited
at spawn, so panes alive across the change are reclassed on adoption where the
OS allows it (R2). Inside tear every thread is spawned with a named class
through one contained seam: gather, parse, input, subscriber and connection
threads at user-initiated QoS; the syncer, persister and checkpoint writer at
utility; history backfill at background. Shells start in the default band
whatever their holder's class: the holder resets its own main thread and
clears inherited background state on the child before exec (R28). kanshou
reports the declared class beside the observed priority.

On the language ladder: the hot paths stay Rust — they make syscalls, are
measured in microseconds and each holds one contained C seam. The
workload-class table is Nix, because it renders units. The case matrix is a
Rust declaration (§6), because its proof is rustc's exhaustiveness. Results
are data, queried with duckdb.

### 4.6 Where the design pass split, and what this record takes

| Question | On the table | Taken | Why |
|---|---|---|---|
| Where the frame codec lives | a new framing crate; tamotsu's framing promoted into `tear_types::wire` | `tear_types::wire`, with the socket profile under `cfg(unix)` | tamotsu already reaches tear-types through makimono (S `Cargo.toml`); mado-web, a wasm32 crate, depends on tear-types (S), so OS calls must stay unix-only |
| Raw frames for bytes | `serde_bytes` only; raw `DATA{at}` frames | both: `serde_bytes` on the legacy CBOR wire (R6), `DATA{at}` on byte lanes (R37) | not for the 2.5 µs per 64 KiB (F-f) but for the offset that must ride every byte frame, in the framer the holder already speaks: one framer, not two |
| How the holder protocol evolves | `PROTO = 2`; capability names with `PROTO` frozen at 1 | names, `PROTO = 1` forever | the holder refuses any other integer (S `holder.rs:206`), so a bump strands every live holder at a daemon upgrade; names decode in all four directions (P) |
| DEC 2026 hold bound | 100 ms (mado today); 150 ms + 2 MiB (Alacritty); 1 s (Ghostty, foot); 2 s (kitty) | 100 ms + 2 MiB, one setting; input modes published ahead of held cells | moving the hold to the authority must not change what the operator sees (S `render.rs:2097`); the heaviest full redraw the codec runs met is 227 KB of VT (C), 3.2 ms even at the background-band ceiling (F-b); a key must not wait out a hold under stale modes |
| Default flush primitive | `F_FULLFSYNC`; `F_BARRIERFSYNC` | `persisted` (`F_FULLFSYNC`), off the byte path; `ordered` a typed option | SESSION-DURABILITY §3.3 promises a bounded loss window; a barrier promises ordering only (`man 2 fcntl`); within either probe a full flush costs 3–17× a barrier (F-c, D); held for the operator (§8.2) |
| Scrollback default | unlimited; 64 MiB per pane | unlimited, stored compactly; 64 MiB proposed | "never lose anything" is a recorded operator contract (S `tear-config` `lib.rs:238`); held for the operator (§8.2) |
| UDS buffer size | 256 KiB; 1 MiB | 256 KiB | already at the plateau, 11.46 against 11.21 µs for 64 KiB (F-a); holds four lane frames |
| Scheduling declaration | a `processType` spec field; a closed workload class | `workloadClass`; a class beside a `processType` wins, named in a warning for that daemon | 32 literal `ProcessType` settings in 27 files across the fleet's Nix (counted 2026-10-07, comments and tests excluded): the third hand-wiring is a primitive; one table renders launchd and systemd; a module assertion would fail every daemon on the node for one daemon's contradiction |
| Matrix language | a tatara-lisp declaration; a Rust macro | a Rust `bench_matrix!` | the proof is rustc's exhaustiveness over tear's enums in tear's workspace, and over mado's and madori's in mado's bench crate; a parsed file would demote it to a CI check (§6) |
| Events | view fields; a ring | a ring with cursors | conflation would drop the second of two notifications |
| Who answers queries | the attached mado; the authority; a lease | a lease per pane: the authority answers while no answering consumer holds it | today a pane no window shows answers nothing and a pane in N windows answers N times (S); a flip that spans two repositories cannot be one commit, but a declaration can change in one |
| A lagging legacy byte subscriber | close it; resync it in band | never close: the journal by offset (held), a capped backlog then an in-band resync (process-bound) | an old mado on its non-switchable path reads a closed stream as the shell's exit, closes its window and kills its session (S mado `gui_tear_attach.rs:741-771, 1253-1268`) |
| Interim DECCKM | a cheap typed modes RPC; mado's mirror | the mirror | the mirror costs no RPC and takes cells and modes from one parser instant, while a separate fetch — today's `pane_cursor_keys_mode` (S `control.rs:311-327`) — can skew; `modes.rs:20-27` states the rule, but nothing enforces it until R38 seals `ModeSet` |
| When readers move to the view | at the flip; onto a projection first | a projection first (R30) | every reader moves in one commit onto one model; the flip then swaps only the producer |
| Lane topology | one multiplexed connection with priorities; one connection per flow-control domain | per domain | the head-of-line blocking measured is in the client mutex and the daemon's serial loop (S), not the socket; a 738 KB message costs 2.98 ms to decode (C) ahead of any key; connect + subscribe costs 0.29 + 0.25 ms once (H) |
| Shared memory | a cross-process ring; UDS | UDS across processes, RCU within one | 7.50–7.54 against 7.08 µs (F-e) |
| Holder upgrade | on every adoption; opt-in, canary first | opt-in, behind a preflight, one canary first | an exec that succeeds into a failing image ends the shell the holder exists to keep; held for the operator (§8.2) |

## 5. The ladder

Rungs are ordered by measured impact over cost within each phase. A rung's
number is its identifier and survives revisions; its phase and its *after*
line give the order, which is why R17 sits in phase B, R18 in phase C, and R42
and R43 where they are needed. Phases B and C touch mostly different
repositories and run side by side. Phase D precedes the holder's protocol
growth (R21, R35's dedup), because holders outlive daemons by design and only
R16 reaches a running one; R4's and R17's fixes need no negotiation and go
earlier, reaching every holder spawned after them. Phase F is SHUKEN's
remaining sequence with the wire under it. *Old behaviour* names the
configuration that keeps today's behaviour reachable when that behaviour is a
good state; where today's behaviour is a bad state it says so, and the rung's
negative control is a fault injector compiled only for tests (§6).

### Phase A — sense first

#### R0 · Tests run where the gate will live
*Prerequisite.*
- **Changes** (extends substrate's cargo CI and tear-types `capability.rs`):
  every test `PaneSnapshot` goes through one constructor, so `--features
  engate` compiles (E0063 today, P); tear's auto-release test gate runs
  `--all-features`; `ci.yml` is re-enabled — disabled at the GitHub level
  since 2026-05-22 after 76 of 76 red runs of the job it replaced, while
  `cargo fmt --check` passes today (S, Actions API); madori, engate, garasu
  and kanshou get the same push and pull-request test job (their tests run
  only at release today, S); the capability vocabulary becomes one
  `capabilities!` row per variant (variant, wire name, advertised, membership
  of `ALL`); the stale models of §10 are corrected in the same change.
- **Effect:** feature-gated tests run on every push, where today they run
  nowhere; a capability missing from `ALL` stops compiling, where today 9 of 9
  tests stay green (P).
- **Gate:** `cargo nextest --workspace --all-features --no-tests=fail` is
  green; red runs: a struct-literal `PaneSnapshot` in a test fails CI with
  E0063, and a variant with no `capabilities!` row fails a `trybuild` case
  with E0004.
- **Old behaviour:** none to keep; this is test infrastructure.

#### R1 · The matrix exists
*Prerequisite · after R0.*
- **Changes** (ports the 2026-10-07 harness and floor suite; extends
  kanshou's introspection surface and mado's `frame_perf`):
  a `tear-bench` workspace member (`publish = false`) carrying the §6 matrix,
  the floors, structural counters behind a `bench-probes` feature, the fault
  injectors that stand in for bad states (§6) and the negative controls; every
  cell starts `Pending` with today's receipt. In mado: paint counters per
  reason, `declined_after_acquire`, a per-method count of tear calls made on
  the UI thread, queue-depth gauges for every channel between tear and the UI,
  parse bytes per tick, and input → present and byte → present histograms on a
  new `kanshou::metrics` (counter, gauge, log histogram); `frame_perf` answers
  *blind*, not zeros, when no GUI is reachable. Two facts the pass lacked are
  measured here first: a parked madori window with no tear link (C10's floor),
  and whether launchd's background state on a running process can be cleared
  from outside it (R2).
- **Effect:** 0 → 13 cases declared; the nine count-metric kinds of §6 gated
  on every push, one cell per case where they apply.
- **Gate:** the matrix compiles with every case; it reproduces the 2026-10-07
  pass's counts exactly (0 of 20 keys at 3,000 rows, 513 allocations per KiB
  of `yes`, 65,537 frames per 64 MiB flood) and its timings as floor multiples
  within §6's noise band (the held flood, the ~1 Hz echo spikes, `yes`
  throughput, RSS per byte flooded); a `trybuild` case with a row removed
  fails with a pinned E0004 or E0063.
- **Old behaviour:** `perf.histograms: off` stops recording; the counters
  stay.

### Phase B — the measured slownesses, smallest change first

#### R2 · The session tree leaves the background band
*Destination · after R1.*
- **Changes** (extends substrate's module-trio `processType`, whose own
  comment already records the band's cost, and `service-helpers`): a closed
  `workloadClass` — `session-host`, `latency-server`, `service`, `background`,
  `xpc-adaptive` — keyed on who waits on the process, rendering launchd
  `ProcessType`, `Nice` and `LowPriorityIO` and systemd `Nice`, `CPUWeight`
  and `IOWeight` from one table; module-trio gains `daemon.workloadClass`,
  defaulted from a spec field; tear declares `session-host` for every daemon
  flavour, because the band slows process-bound panes too. A daemon that
  declares a class gets its scheduling from the class alone: an explicit
  `processType` beside it is not rendered, and that daemon's evaluation warns
  with both values, so the contradiction never reaches a unit and every other
  daemon on the node evaluates unchanged. A daemon that declares no class
  keeps today's free `processType`, unchanged: every explicit setting already
  in the fleet — engenho's on darwin servers among them — and module-trio's
  own tests (S substrate `module-trio-test.nix:325`) evaluate as today, and
  the fleet's hand-set sites migrate later. Bands are inherited at spawn (F),
  so a pane alive at the rebuild would keep the background band until its
  shell exits: on adoption the daemon clears inherited background state on
  each holder, its shell and the shell's live descendants —
  `setpriority(PRIO_DARWIN_PROCESS, pid, 0)`, the inverse of the call F used
  to set it, through the seam crate R28 grows, which this rung introduces with
  its first call — if R1 finds launchd's state clearable from outside;
  otherwise kanshou reports those panes as declared `session-host`, observed
  background. On Linux, held panes run with `KillMode=process` inside the
  daemon's unit (S tear `flake.nix:57-63`), so the unit's weights cannot
  favour the daemon over a build in one of its panes: each holder moves into
  its own transient scope through the user manager, and the class's weights
  apply to the daemon alone.
- **Effect:** with the daemon and holder at PRI 4 and the client at PRI 31, as
  `Adaptive` leaves them (H): held flood 19.3 → 54.0 MiB/s, held `NewSession`
  218.6 → 23.8 ms, the mado-shaped key 7.6–8.6 → 3.46 ms until R5 removes its
  RPC, warm held echo 108.7–136.0 → ~32 µs (H-b); `sleep(1 ms)` 19.7 → 1.27 ms
  (F-timers); a CPU-bound loop in a pane 8.3× faster (R, probed on the
  reference Mac). When mado itself is App-Napped the client sits at PRI 4 too,
  which this rung does not touch: 8.4 MiB/s and 18.2 ms a key in that variant
  (H).
- **Gate:** module-trio rows — every class renders on darwin and linux, and
  `session-host` beside `processType = "Background"` renders `Interactive` and
  warns (an evaluation test with pinned output, the red run of §8.1's band
  row); after a rebuild, the daemon, every holder and each pane shell run at
  thread priority ≥31, read from `proc_pidinfo` because `getpriority` is blind
  here (F), reported separately for panes spawned after the rebuild and panes
  adopted from before it; on Linux, warm echo p50 within 1.5× and p99 within
  3× of the quiet run while `make -j` runs in another pane; the negative
  control `Band::Background` keeps the C2–C4 throughput, C2 and C4 warm-echo,
  C8 `NewSession` and C12 key cells red.
- **Old behaviour:** `workloadClass: xpc-adaptive` renders today's `Adaptive`
  exactly and stays module-trio's default, so no other daemon changes band.

#### R3 · The wire stops losing things
*Correctness prerequisite · after R1.*
- **Changes** (extends the daemon's dispatch, tear-client's `rpc`,
  `replayable` and `subscribe`, and mado's sinks): the daemon estimates a
  response's encoded size before cloning (rows × cols × 61 B for a snapshot,
  from the 60 B per cell H measured) and answers
  `WireError::Rejected("response-too-large: …")`, the string variant old
  clients decode (S `wire.rs:445-452`), carrying a typed reason only on a
  negotiated variant (R15); tear-client never reads from a connection left
  mid-frame, never replays a deterministic failure and replays only idempotent
  reads; mado's PTY and resize sinks stop discarding `send_keys` and resize
  errors (S `gui_tear_attach.rs:868-881`); subscriptions dial through
  `Client::dial`, which authenticates, and then probe, which sends `Hello` (S
  tear-client `lib.rs:333-349, 392-421`); a re-dial re-sends the client
  identity and re-probes capabilities, where today it keeps the view probed at
  the first connect (S `lib.rs:651-655`); paste and `SendKeys` travel in 64
  KiB chunks with typed errors, sent back to back, and a chunked paste that
  fails closes its own bracket with ESC[201~ (best effort; R23 makes it the
  writer's job).
- **Effect:** keys in a 3,000-row pane 0 of 20 → 20 of 20, and each failing
  DECCKM attempt 126–128 ms with two full encodes → one typed error in <1 ms
  (H); a paste above ~8.1 MiB arrives instead of vanishing (W); tokened
  daemons stream bytes; Leader-gated panes take keys after a daemon restart.
- **Gate:** the key-loss row (3,000 rows: 20 of 20, 0 reconnects), a
  tokened-subscription row and a 16 MiB paste hashed in the child, each red on
  today's code.
- **Old behaviour:** none to keep; this removes failures.

#### R4 · A holder never mutes a pane
*Destination · after R1.*
- **Changes** (extends tamotsu's sink handling in `holder.rs`, `follow` and
  `repair` in `held.rs`, and the session store): a holder's sink owns its
  connection, and dropping it — on a write failure or a timeout — shuts the
  connection down both ways, where today the sink is dropped while the socket
  stays open (S `holder.rs:181-183`, D). The daemon already reads that EOF as
  "re-attach with `Attach{from: next}`" (S `held.rs:165-182, 198-211`, PROTO
  1), so for a new holder the shutdown is the whole fix. For holders older
  than this rung, a key with no echo within 2 s, and every snapshot or MCP
  read, makes the daemon compare `Status.end` with its received offset over a
  fresh connection and re-attach — an edge, not a poll. Re-attaching would
  turn two daemons on one store into a livelock, each displacing the other
  (SESSION-DURABILITY §7 records the case), so the store gains an authority
  lease: a daemon takes it at start with an incarnation number persisted
  beside it, only the lease holder attaches, repairs or re-attaches holders,
  and a daemon that loses it stops every follow loop; `Attach` gains an
  additive `incarnation`, and a holder from this rung on refuses an older one
  with a typed `Displaced{by}` that ends that daemon's repair loop for the
  pane; a daemon older than R4, which cannot decode `Displaced`, is left
  attached and silent, as a second attach leaves it today, rather than set
  spinning.
- **Effect:** after a >2 s stall the reproduction received 7,259 B in 3 s on
  an open socket while the journal grew 2,621,440 → 5,505,024 B (D); target 0
  B lost, output resumed by offset.
- **Gate:** stall the daemon's reader 3 s under a 64 KiB / 50 ms producer:
  delivered bytes equal journal growth; a `Prev` holder muted the same way
  recovers through the `Status.end` check (a §6 compatibility cell); two
  daemons on one store for 10 s make at most two attaches per pane, and the
  loser logs `Displaced`; the `bench-probes` fault that keeps a failed sink's
  socket open is red.
- **Old behaviour:** none to keep: a mute pane is a bad state. Holders spawned
  before R4 keep today's sink handling until their shell exits; the
  daemon-side check covers them.

#### R17 · Forward first, flush beside
*Destination for where the flush runs · after R1 · the default primitive is
held for the operator (§8.2).*
- **Changes** (extends makimono's `Journal`, `JournalBounds` and `atomic.rs`,
  tear-config's `JournalConfig`, tamotsu's `holder.rs`, praça persistence):
  the journal splits into a writer that appends to the page cache and a syncer
  thread that parks until dirty and group-commits outside every lock; segment
  rotation hands the closed segment's flush to the syncer, and eviction
  unlinks off the byte path, where today both run inline in `append` under the
  holder lock (S `journal.rs:128-135, 145, 214-238`); the holder forwards,
  then appends, then marks dirty, and a flush precedes a forward only under
  the named `write_ahead`; a tail ring keeps offset cursors lossless.
  `journal.sync` is typed by what each guarantee promises: `page_cache`,
  `group_commit{guarantee, interval: NonZero ms, max_unsynced: NonZero bytes}`
  or `write_ahead{guarantee}`, where `guarantee` is `persisted` (macOS
  `F_FULLFSYNC`, Linux `fdatasync`), `ordered` (macOS `F_BARRIERFSYNC`; Linux
  has no ordering-only primitive, so it runs `fdatasync` and logs the upgrade)
  or `handed_to_device` (macOS `fsync(2)`; Linux `fdatasync`, which promises
  more). It defaults to `group_commit{persisted, 1 s, 1 MiB}` — exactly the
  guarantee SESSION-DURABILITY §3.3 states, now off the byte path; the legacy
  `fsync_interval_ms` still parses (0 → `write_ahead{persisted}`, N →
  `group_commit{persisted, N}`; today 0 flushes on every PTY read, S
  `journal.rs:149-158`). Every rename of a tombstone or a `session.json` is
  followed by a flush of its directory, which `atomic.rs` lacks today (S
  `atomic.rs:16-33`), so a crash cannot drop the tombstone of a session a
  human ended. Documents and pane metas go through one debounced writer off
  request threads, tombstones staying synchronous; the holder's tick hands its
  two duties on — the flush to the syncer, the Linux cwd poll to the syncer's
  wake (R18); attach stops flushing before replay; calls go through nix's safe
  `fcntl`, so makimono keeps `forbid(unsafe_code)`.
- **Effect:** first echo after a pause, holder hop: p50 7.6 / p90 25 / max 38
  ms → ~0.24 ms (D, n=15 each); echoes >3 ms per 15 s series 19–24 → within
  twice the same run's `page_cache` series, whose flush-free relatives read
  1–2 (H-c); held flood ≥ the no-flush variant's 56.5 MiB/s (H-d); holder
  timer wakeups 4.5/s by the code (a 250 ms tick plus a 2 s cwd poll; L
  measured 2.8 context switches/s) → 0 (S); held `NewSession` sheds two or
  three synchronous flushes, ~7.5–12.6 ms (F-c, S).
- **Gate:** the held typing series shows no inter-spike gaps at 990–1,040 ms,
  echoes over 3 ms at most max(4, 2× the same run's `page_cache` series), and
  p90 ≤1.5× that series; pause-then-key over the full chain p50 ≤1.5× the same
  run's `page_cache` variant; a pump-thread counter reads 0 flushes and 0
  unlinks; a NixOS VM test resets the guest mid-flood (`machine.crash()`):
  loss is at most the interval under `persisted` and exceeds it under a
  `page_cache` control, and a tombstone written before the reset survives it;
  the control `write_ahead{persisted}` is red. Whether an in-flight
  `F_FULLFSYNC` still stalls appends to the same file in the kernel is
  unmeasured; this gate decides it, and a red here goes to the operator with
  `ordered` as the measured alternative.
- **Old behaviour:** none for the inline-when-due placement: it buys the same
  loss window as `group_commit` at the price of the spikes, so it is a bad
  state; `write_ahead{persisted}` (durable before visible) is the stricter
  good state and the negative control; the legacy key still parses. Holders
  spawned before R17 keep inline flushes until their shell exits.

#### R5 · Keys never wait on a snapshot
*Interim on the path · after R1 · replaced by the view's modes at R38.*
- **Changes** (extends `ModeSet`, `PaneGrid`'s mode handling,
  `PaneSnapshot::to_ansi`, the daemon's subscribe, engate's attach and mado's
  mirror): tear tracks every mode a key encoder reads — the kitty keyboard
  flag stacks (one per screen, as the protocol specifies), modifyOtherKeys,
  DECKPAM, DECSCUSR, DECSCNM, 1007 and 1015/1016, none tracked today
  (`ModeSet` has nine fields, S `modes.rs:125-135`; no kitty-flag state in
  tear-core or tear-types, two probes). `to_ansi` emits the complete
  `ModeSet`, where today it emits only `?1049h` and `?25l` (S
  `pane_snapshot.rs:372-480`), and emits a stack-valued mode as an absolute
  restore — `CSI < 99 u` to empty the stack, then each push, per screen — so a
  replay applied twice equals one. One replay source per attach, where today
  every daemon attach replays history twice, the second copy through the
  answering path (S daemon `lib.rs:1002-1016`, engate-attach `lib.rs:194-212`;
  Gate 0 class 8): the daemon's first `PaneBytes` after the `Subscribe`
  acknowledgement is the replay, mado feeds it silently, and engate skips its
  own snapshot RPC for a producer that declares stream-carried replay;
  `to_ansi` lays history down once, without the `rows` blank lines it scrolls
  past today (S `pane_snapshot.rs:386-399`). The daemon registers a subscriber
  and takes its snapshot under the pane's grid lock, with the fan-out ordered
  grid then subscribers, so no byte lands both in the replay and in the stream
  (R20 generalizes this fence to offsets). mado's replay applies the
  snapshot's `ModeSet` to its mirror; the `cursor_keys_mode` closure (S
  `gui_tear_attach.rs:891-899`) and alt-scroll read the mirror, as kitty flags
  and bracketed paste already do. Capability `replay-modes`.
- **Effect:** the mado-shaped key drops to a plain key's echo at every depth:
  3.41 ms → 0.12–0.16 ms p50 at 0 rows, and 91.4 ms of snapshot RPC → a plain
  key's echo at 1,453 rows (H); 469,852 → 36 B per key (H); alt-scroll in
  `less` stops pulling a snapshot per step; mouse, paste and focus modes are
  right after attach and switch; one history replay per attach instead of two.
- **Gate:** the mado-shaped key p50 is ≤1.25× the same run's plain-key echo at
  0, 1,453 and 3,000 rows; an attach-into-vim row asserts DECCKM, 2004 and
  1000/1006 restored (red today); replay-twice equals replay-once for every
  `ModeSet` field; attach mid-neovim, quit it, and Ctrl-C reaches the child as
  0x03 under `Prev` and `Head` mado; an attach to a pane holding N scrollback
  rows leaves the consumer exactly N rows (red today) and writes 0 answers to
  the PTY; the control `daemon-rpc` reddens exactly the RPCs-per-key and
  bytes-per-key cells.
- **Old behaviour:** `input.cursor_keys_source: daemon-rpc`, chosen
  automatically against a daemon without `replay-modes`.

#### R42 · One answerer per pane
*Destination · after R5 · the multi-window half needs R15; the post-flip
declaration lands at R38.*
- **Changes** (extends `PaneGrid`'s host role (S `pane_grid.rs:1610-1631`),
  `InProcess`'s fan-out, the daemon's subscribe, tamotsu's `follow`, mado's
  response writer): a per-pane answering lease, read and changed under the
  pane's grid lock in R5's fence order, so each query is parsed against one
  answer to "who answers". A legacy subscription — the wire's `Subscribe` or
  `InProcess`'s byte subscription — holds it, because every mado today answers
  what its window displays (S mado `gui_tear_attach.rs:677-695`). With the
  lease empty the host role is on, and `take_response` — which nothing outside
  tests drains today (S `pane_grid.rs:1625`; its callers sit at 2189-2277) —
  is drained after each feed into a per-pane reply writer that writes outside
  every lock and never blocks the parser (the reply half of R23's writer; for
  a held pane it sends the holder `Write`). A replay below an adoption
  boundary answers nothing: `Status.end` at attach for PROTO-1 holders, and
  from R15 on a holder-recorded `delivered_through`, so queries journaled
  while no daemon was attached are answered exactly once. From R15 on,
  `SubscribeWith` declares `answers_queries`; with two answering consumers on
  one pane the daemon names one and pushes the flag, which a new mado honours
  while an old one keeps today's duplicate, scoped to that pane. Answers that
  describe a renderer (DA1's parameters, cell size, graphics and keyboard
  protocols) come from what the last viewer declared (R43) or a configured
  default.
- **Effect:** a pane nobody watches answers DSR, CPR and DA at once, where
  today each query waits out the asker's timeout — 2 s per cursor report for a
  crossterm-based line editor (R: crossterm 0.28.1 `cursor/sys/unix.rs:39`),
  and a reedline prompt can send more than one (S pleme-io reedline
  `painter.rs:361-372`) — after which pleme-io's reedline fork, which frost
  builds against, falls back to a guessed prompt row (S reedline
  `painter.rs:204-224`); two windows on one pane answer once instead of twice
  (new mado).
- **Gate:** a matrix of {0, 1, 2 viewers} × {`Prev`, `Head` mado} × {attach,
  detach, daemon restart mid-query} asserts exactly one reply per query (the
  zero-viewer cells are red today); a re-adoption replay of a journal holding
  CPRs writes 0 answers; the `bench-probes` fault that disables the lease
  reddens exactly the zero-viewer cells.
- **Old behaviour:** none to keep: an unanswered or doubly answered query is a
  bad state.

#### R6 · Bytes as bytes
*Destination for the legacy wire · after R1.*
- **Changes** (extends tear-types `wire.rs` and `graphics.rs`): `serde_bytes`
  on `PaneBytes`, `SendKeys.bytes` and `Graphic.data`. No capability: ciborium
  decodes either form in both directions for `PaneBytes` and `SendKeys` (8 of
  8 shapes, 0 failures, P); `Graphic.data`, nested inside `PaneSnapshot`, was
  not probed and joins both the probe and this gate.
- **Effect:** wire bytes per output byte 1.98 → 1.014 at today's 1 KiB frames
  (1,038 B per 1,024, F-f) and 1.0002 at 64 KiB (65,552 B per 65,536, P); a 64
  KiB round trip 724.7 → 4.18 µs, 1 KiB 11.94 → 0.200 µs, 64 KiB in the
  background band 5.84 ms → 23.7 µs (F-f); the stream ceiling ~105 MiB/s →
  ≥1.1 GiB/s (W); an 8 MiB-history replay frame 20.8 MB (rejected, H-f) →
  ~10.4 MB (accepted).
- **Gate:** a 64 KiB `PaneBytes` frame is 65,552 B and round-trips within 3×
  the same run's raw lean 64 KiB frame (serde_bytes measured 2.5×, `Vec<u8>`
  434×, F-f); cross-version decode rows against the published tear-types,
  `Graphic.data` included; the test-only array encoder is red.
- **Old behaviour:** none to keep: the same values in another encoding.

#### R7 · One split-safe feeder
*Correctness prerequisite · after R1.*
- **Changes** (extends mado's `incomplete_utf8_tail_len` and feed, tear's
  `ApcScanner`, espelho): one feeder for both parsers never hands a parser an
  incomplete UTF-8 tail or a lone trailing ESC, holds back an incomplete
  escape up to a bound, and passes everything else through borrowed;
  `PaneGrid::feed` takes a `Chunk` only the feeder mints; espelho gains
  `feed(whole) == feed(any split)`.
- **Effect:** UTF-8 split loss in 3 of 3 crafted split cases → 0 (G; the cause
  is vte 0.15.0's `advance_partial_utf8`, S); the per-chunk copy, 0.56–0.70 ns
  per byte against vte's own 0.51–1.32 (G), becomes a borrow; offsets land on
  sequence boundaries, which R20 and R27 need.
- **Gate:** the espelho split-invariance proptest over arbitrary UTF-8 and the
  query catalog (red today); a `trybuild` case minting a `Chunk` outside the
  feeder fails with E0451.
- **Old behaviour:** none; the old path survives as the test oracle.

#### R8 · Sockets sized, one write and one read a frame, no Nagle
*Interim on the path · after R1 · absorbed by R19's framer.*
- **Changes** (extends `write_msg`, `read_frame`, the daemon's accept loops,
  tear-client's dial and tamotsu's sockets): one buffer or `writev` per frame;
  a per-connection read buffer on the daemon, which today reads the raw
  stream's header and body separately (S daemon `lib.rs:809`,
  `wire.rs:574-585`), so a frame smaller than the buffer is one read;
  `SO_SNDBUF`/`SO_RCVBUF` of 256 KiB on every tear UDS; `TCP_NODELAY` on every
  TCP socket; a blocking TCP accept, woken by a self-connect on stop.
- **Effect:** a 64 KiB round trip 77.5 → 11.46 µs at 256 KiB (11.21 µs at 1
  MiB), UDS throughput 1.6 → 29.8 GB/s, and ~3.2 µs per round trip of framing
  overhead gone (F-a); a 738 KB frame 558 → 62 µs (W); TCP connect 29.6 ms →
  <1 ms and subscribe 53.6 → <2 ms (H).
- **Gate:** a `getsockopt` row for every socket kind; daemon syscalls per RPC
  4 → 2 (4.24 measured, L); the TCP connect and subscribe rows; the controls
  `os-default` and two-write are red.
- **Old behaviour:** `transport.uds_buffer: os-default`.

#### R9 · Screen-first reads
*Interim on the path · after R3 · these requests become the bulk lane's at
R37.*
- **Changes** (extends `PaneSnapshot`, `to_ansi`, tear's MCP, tear-client and
  mado's attach): `Keyframe{pane, history}` (screen, modes, tables and an
  optional bounded window) and `HistoryRange{pane, rows}` (≤1,000 rows a page,
  bounded at decode), each in two encodings — packed rows, the view codec
  (runs keyed by style and link, an ASCII fast path, trailing blanks trimmed,
  chunks ≤64 KiB), for renderers and observers; and VT, `to_ansi` with the
  complete `ModeSet` and the primary screen laid down under an alternate one,
  which today it drops (S `pane_snapshot.rs:372-399`), for byte consumers and
  for mado's mirror before the flip, which asks for no more history than it
  keeps. MCP `pane_snapshot_text`, the snapshot CLI and mado's attach use
  them; tear-client answers `pane_cursor_keys_mode` from `Keyframe{history:
  none}`, so no client pulls a full snapshot to read one mode (the trait
  default does, S `control.rs:311-327`). Capability `snapshot-range`. The
  legacy `PaneSnapshot` keeps full semantics for old clients, behind R3's
  typed refusal.
- **Effect:** a 163×48 snapshot 469,239 → 3,850 B for a build log (3,312–6,368
  B across the text corpora, 63 B blank, 87,351 B per-cell truecolor), encode +
  decode 2,490 → 24.7 µs, lossless with wide and combining characters (C);
  MCP reads stop failing past ~1,667 rows at 163 columns and at 1,000 rows at
  300 columns (H, M); no read on these paths approaches 16 MiB.
- **Gate:** a codec proptest (lossless, every frame ≤64 KiB) over random grids
  up to 500×200; snapshot bytes flat at 0, 1k, 10k and 100k rows; the
  `bench-probes` fault `snapshot.history: all` is red at 3,000 rows.
- **Old behaviour:** the legacy `PaneSnapshot`.

### Phase C — edges, not ticks

#### R10 · Output wakes the window; a pane's end is an edge
*Destination · after R1.*
- **Changes** (extends madori's `run_with_user_events` and `user_event` (S
  `app.rs:494, 722`), engate-attach's `poll_one`, mado's `stream_watch.rs`):
  madori owns one event-loop proxy and hands out a coalescing
  `std::task::Waker` that sends only when its pending flag goes false → true,
  the loop clearing the flag before it drains, because each proxy clone adds a
  run-loop source (S winit 0.30.13); every mado producer takes the waker in
  its constructor; engate-attach gains `Attach<Live>::poll` answering item,
  empty or closed, beside today's `poll_one`, which returns `false` for both
  empty and closed (S engate-attach `lib.rs:281-290`) and stays for existing
  consumers; mado's stream-watch relay thread goes away (S
  `stream_watch.rs:44-59`), while tear-client's subscribe thread stays as the
  socket reader; pane fate is read on the stream's end — `PaneClosed` is
  already sent on exit and on kill (S `inproc.rs:737-738` and `:825`; the
  daemon writes it at daemon `lib.rs:1029-1031`) — with a 30 s backstop; the
  per-event block runs on wake or redraw only.
- **Effect:** output → loop wake 0–16.7 ms (mean 8.3 under `Capped(60)`, S) →
  48.8 µs p50 / 712 µs p99 from idle (F-d); idle `get_pane` 54–61/s → 0, and
  with it ~90 % of the daemon's CPU (8.79 of 9.73 s, L).
- **Gate:** the UI-thread `get_pane` counter reads 0 at idle; a madori row: a
  ring while parked yields a redraw within one loop turn; a loom model of the
  waker shows no lost wake when a ring races the drain; byte → present p50 <2
  ms for an isolated echo; the control `tear.pane_fate: poll` and the
  `bench-probes` fault that disables the wake are red.
- **Old behaviour:** `tear.pane_fate: poll`; `tear.fate_backstop_secs` (0
  turns the backstop off).

#### R11 · Frames on demand: Parked, Hot, Hidden
*Destination · after R10.*
- **Changes** (extends madori's `FramePacing::Reactive` and `FrameDebt`,
  garasu's adaptive caps): `RenderCallback::frame_demand` returns `Idle`,
  `Now`, `At(t)` or `Continuous`, and its default maps `needs_frame`, so other
  madori consumers see no change; the first frame after idle renders at once
  and later ones at most once per refresh, late-latched; two idle ticks, then
  park; occluded and minimized windows are Hidden (no acquire;
  `FrameDebt::Revealed` owed). On Wayland, which has no occlusion event (§1),
  Hidden is inferred from an outstanding frame callback: madori calls
  `pre_present_notify` and lets the compositor's callbacks pace it — R14's
  Wayland arm lands here — because an acquire on a hidden surface under
  `AutoVsync` may block the UI thread (unmeasured, §8.3). Pointer motion alone
  never keeps a window Hot. mado's blink, bell decay, fades, kinetic scroll,
  board tick and reattach backoff become deadlines; an open search bar
  repaints only on change. A timer at the screen's maximum rate is the vsync
  source until R14.
- **Effect:** idle ticks 57.83/s → ≤1/s and GUI idle CPU 1.11–1.30 % → ~0 (L);
  output at the panel's refresh instead of 60 Hz (S `config.rs:2702`); 0
  acquires while occluded, where today every change renders (S madori
  `app.rs:1394`).
- **Gate:** ≤1 tick/s over 60 quiet seconds; a state × event matrix over a
  fake surface (no acquire while Hidden, `At(t)` honoured, the kinetic glide),
  run again on a headless Wayland compositor; the control `performance.pacing:
  capped` is red.
- **Old behaviour:** `performance.pacing: capped` or `continuous` reproduces
  today exactly.

#### R12 · Never present an unpainted drawable
*Destination · after R11.*
- **Changes** (extends madori's render callback and mado's `render.rs`): a
  `StagedRender` whose `prepare` may decline and whose `encode` cannot,
  because `Encoded` is minted only by `PaintTarget::finish`; the DEC 2026
  deferral becomes `FrameDemand::At(bsu + 100 ms)`, decided before acquiring,
  where today it is decided after (S `render.rs:6791-6808`); the effect-chain
  failure draws the scene straight to the surface; the scrub constants (S
  `render.rs:2107, 2133`) become `render.swapchain_scrub_frames`, 0 once the
  A/B passes.
- **Effect:** painted frames per sparse change 4 → 1; repeat paints 74.1 % →
  ≤5 % (L; the attribution is likely rather than exact, since the late gate
  lacks the selection and blink terms, S); 2.7–10.9 ms of UI-thread CPU saved
  per echoed key at the two last-frame readings, 886 and 3,634 µs (L).
- **Gate:** a fake-surface ledger (every present follows a paint into that
  drawable); a BSU split across dispatches acquires nothing until ESU or the
  deadline (red today); repeat paints ≤5 % of painted frames over the idle
  script; a live A/B with scrub 0 on macOS and Linux shows no stale content;
  `declined_after_acquire` stays 0 for a day.
- **Old behaviour:** `render.swapchain_scrub_frames: {after_content: 3,
  after_epoch: 3}`.

#### R13 · The UI thread holds only a link
*Destination · after R5, R10.*
- **Changes** (extends tear-client, engate's `Attach<Live>`, mado's sinks,
  picker and paste path): `tear_client::link` returns a `LinkHandle` whose
  methods only enqueue — input, paste, resize into a latest-value slot,
  requests, attach preparation — and a mailbox of typed `LinkEvent`s; an input
  thread is the window's only writer to tear (FIFO; paste in 64 KiB chunks
  inside its brackets, which the same thread closes with ESC[201~ on cancel or
  link loss; motion deduplicated per cell and wheel steps summed; bounded,
  overflow a typed refusal); a control thread; the view lane is engate's
  `Attach<Live>` on its own thread, parsing into the mirror; the picker lists
  from one `ListSessions`; Ctrl-D, create and praça writes leave the UI
  thread. The link is generic over `MultiplexerControl`, so C1 and C2–C6 share
  it; a `thread_init` hook lets mado class its threads (R28).
- **Effect:** blocking tear calls on the UI thread → 0; per key, UI time 3.41
  ms (PRI 31) or 7.6–8.6 ms (daemon and holder at PRI 4) → one enqueue and one
  notify, ~1.2–2.3 µs, the input thread waking ~6 µs later (H-b; F-e, F-d); a
  paste of any size blocks the UI for 0 ms, where today an 8 MiB paste blocks
  it ~0.3 s while the child drains it (27.4 MB/s, F-b) and anything above ~8.1
  MiB is lost (W); hovering a tracking TUI, 60–120 blocking `SendKeys`/s plus
  up to ~240 `GetPane`/s (S, rate estimated) → ≤1 report per cell change; no
  VT parse on the UI thread.
- **Gate:** a scripted session (1,000 keys, a 16 MiB paste hashed in the
  child, a Ctrl-S switch, Ctrl-D, a resize drag, a hover over a mouse-tracking
  TUI) keeps the UI-thread tear-call counter at 0, UI dispatch p99 ≤4 ms
  through a 64 MiB `cat` too, and ≤1 mouse report per frame; a structural test
  refuses `MultiplexerControl`, `tear_client::Client` and `InProcess` in UI
  modules; a `trybuild` case calling a blocking method on `LinkHandle` fails
  with E0599.
- **Old behaviour:** none to keep for the blocking calls: a UI thread blocked
  on tear is a bad state, and the `bench-probes` fault `ui_io: inline` is the
  negative control; `input.mouse_motion: every_event` keeps per-event motion.

#### R14 · Presents locked to the panel
*Destination · after R11.*
- **Changes** (extends madori's scheduler, through objc2-app-kit and
  objc2-core-video, already in mado's lock): a `VsyncSource` chosen at run
  time — `CADisplayLink` on macOS 14+ (common run-loop modes; a frame-rate
  range up to the screen's maximum while Hot), `CVDisplayLink` on 11–13,
  Wayland frame callbacks (landed with R11), a timer as fallback; one link per
  display for the process's life, re-bound on move and scale change; the
  callback only rings the doorbell; madori's one unsafe seam under
  `deny(unsafe_code)`.
- **Effect:** ≤60 unaligned presents/s → one aligned present per refresh at
  the panel's maximum (up to 120 Hz on the reference Mac, whose rate was not
  read, L); 0 link callbacks while Parked; the minimum macOS stays 11.0.
- **Gate:** presents/s through a 10 s flood ≥0.98× the screen's maximum
  refresh read from `NSScreen` at run time, with the inter-present mode at one
  refresh interval; the reported source matches the OS; 0 callbacks while
  Parked; a CI row forces `performance.vsync_source: cvdisplaylink` on the
  macOS 14 and 15 runners, the only gate that executes the path macOS 11–13
  runs.
- **Old behaviour:** `performance.vsync_source: timer`.

#### R18 · The authority sleeps when nothing happens
*Destination · after R17.*
- **Changes** (extends tear's `main.rs`, the persister in `durable.rs`, the
  registry, praça): the daemon's main thread blocks on a signal channel
  instead of a 200 ms loop; the persister parks on a registry generation
  counter and a cwd-changed signal, debounces 250 ms, writes only dirty
  sessions and reads cwd from a per-pane value the parser updates on OSC 7 —
  the published frame once R26 lands — so it takes no grid lock, ending the
  nesting of the global grids lock over each pane's; praça gets one writer,
  ending the race in which two connection threads share one per-process temp
  file (S `atomic.rs:51-58`), and a binding takes its cwd from the request's
  `SpawnEnv` instead of the daemon-global spawn cwd (S daemon
  `lib.rs:1500-1509`), which resident mado never sets because it sends a
  per-request environment (S mado `gui_tear_attach.rs:259-270`); the holder's
  cwd poll rides the syncer's wake; macOS cwd comes through R28's seam.
- **Effect:** authority timer wakeups 5/s + 2/s + 4.5/s per held pane (+20/s
  with `--tcp`) → 0 (S); the daemon's 77 context switches/s → ≤1/s with no
  client polling (L); kill and rename RPCs shed 4–9 ms of flushing (S);
  resident sessions get praça bindings.
- **Gate:** over 60 s with no clients, the daemon makes ≤60 context switches
  and each holder 0 (`proc_pidinfo` deltas); an rg gate refuses
  `thread::sleep` in daemon and holder loops; a resident `NewSession` in a
  project directory creates its binding.
- **Old behaviour:** `persist.debounce_ms`.

### Phase D — negotiation both ways, and the holder's reach

#### R15 · Negotiation both ways; state readable both ways
*Prerequisite · after R0.*
- **Changes** (extends `Capability`, `Request::Hello`, the daemon's serve
  loop, tamotsu's Hello, makimono's `store.rs`, tear-config): `Hello` gains
  `client_capabilities` and `client_id` (`serde(default)`); an appended
  `SubscribeWith{pane, accepts, from}` gated by `subscribe-with`, since
  `Subscribe(PaneId)` is a newtype variant and cannot grow; the serve loop
  splits into a `LegacySink` (`PaneBytes`, `PaneClosed`) and a
  `NegotiatedSink<Accepts>`, so pushing a new frame to a peer that never
  accepted it is E0599; holder Hellos carry capability names both ways while
  `PROTO` stays 1 under a const assert, and a holder from this rung on records
  `delivered_through`, the highest offset it handed an authority sink, in
  `Status`; `SetConfig` carries the writer's tear-config schema version, and
  the daemon merges onto the keys that version knows (§7 rule 7); session
  documents are read at any version up to the newest known, unknown fields
  ignored, where today the check is exact equality (S `store.rs:184`).
- **Effect:** every later wire rung ships in one release with no flag day; a
  rolled-back daemon reads a newer daemon's sessions; an older mado's impose
  no longer resets keys it cannot see.
- **Gate:** the §6 compatibility matrix; a `trybuild` case pins E0599 on
  `LegacySink`; holder Hellos decode in all four directions (P); an impose
  from `Prev` mado leaves every `Head`-only key unchanged.
- **Old behaviour:** the untagged legacy paths are served for good.

#### R16 · Holders upgrade in place
*Prerequisite · after R15 · its default is held for the operator (§8.2).*
- **Changes** (extends tamotsu's spawn in `held.rs` and the holder's control
  verbs): holder capability `reexec`. On `Upgrade{program}` the holder first
  runs `<program> hold --adopt-abi` as a child and requires it to report an
  adoption ABI the holder speaks; it then quiesces the pump and appends every
  byte already read to the journal, so the journal's end covers every
  forwarded offset; clears close-on-exec on the PTY master and its listener;
  and execs the new holder with adoption arguments that carry the ABI version
  and R35's dedup table — same pid, so the shell stays its child; a failed
  exec leaves the old image serving. The daemon clamps its offset to
  `Status.end` on every reconnect, where today it does so only after a revival
  (S `held.rs:198-211, 239-244`). Daemon setting `durability.holder_upgrade:
  never | canary-then-all`: one holder upgrades first and must answer `Hello`
  with an unchanged child pid and contiguous offsets before the rest follow,
  and the rollout stops at the first failure.
- **Effect:** every later holder-side change (R21, R35's dedup) reaches every
  pane created after this rung, including shells that live for months, once
  the operator opts in. Holders from v0.1.28 — the first, shipped 2026-10-06
  (S) — up to this rung cannot be upgraded and keep their behaviour until
  their shell exits.
- **Gate:** an upgrade under a running echo loop keeps journal offsets
  contiguous, the child pid unchanged and 0 bytes lost; a rollback (`Head` →
  `Prev`) is refused at the preflight or completes with the shell alive; an
  image that panics at startup is caught by the preflight and the shell stays
  with the old image; the control `never` leaves the holder's version
  unchanged.
- **Old behaviour:** `durability.holder_upgrade: never`, the default until the
  operator decides (§8.2).

### Phase E — the authority's data path

#### R19 · One framer
*Destination · after R8, R15.*
- **Changes** (extends tamotsu's `proto.rs:167-201` into tear-types
  `wire.rs`): one frame shape for every tear socket, `[u32 BE len][u8
  tag][payload]` with tags `CONTROL` (CBOR), `DATA` (`[u64 at][bytes]`),
  `VIEW`, `INPUT` and `BULK`; the untagged CBOR form stays a typed mode for
  the session connection and old peers; one vectored write per frame
  (`IoSlice::advance_slices`, since `write_all_vectored` is unstable);
  header-then-body reads from one buffered reader into reused buffers; a
  writer-side cap that refuses before writing; R8's socket profile moves here
  under `cfg(unix)`, with nix as a unix-only dependency.
- **Effect:** the holder's decode 3.33 → 0.67 µs per 64 KiB (round trip 5.44 →
  1.67 µs, F-f); 11.5 → 19.4 GiB/s at 64 KiB frames (D); two framers with the
  same length prefix and cap become one — duplication rather than convergence,
  since tamotsu's was written beside `wire.rs` on 2026-10-06, and their shapes
  differ in the tag byte, the zero-length check and the side that enforces the
  cap (S).
- **Gate:** tamotsu's protocol tests stay byte-identical (PROTO 1 holders
  outlive daemons); a raw 64 KiB frame round-trips within 1.5× the same run's
  lean floor; an rg gate refuses raw `TcpStream::connect` and
  `TcpListener::accept` outside the profile.
- **Old behaviour:** untagged framing on every connection that did not
  negotiate `lanes-v1`.

#### R20 · One coordinate: the pane's output offset
*Destination · after R7, R10, R15.*
- **Changes** (extends makimono's absolute offsets, tamotsu's `Attach{from}`,
  engate's `Producer`, SHUKEN's `GridEpoch`): `on_bytes(at, bytes)`
  everywhere, `at` coming from the holder or `PtyHandle::bytes_consumed`;
  `PaneGrid` records `fed_through`; `GridEpoch{incarnation, gen, at}` stamps
  frames, deltas, events and byte frames; attach is atomic under the pane lock
  — a keyframe at N with cursors registered at N; engate gains
  `Producer::attach(at)`, returning history at N and a bounded, wakeable lane
  from N whose poll is R10's three-state poll; `SubscribeWith` carries no
  stream-carried replay — its keyframe at N arrives typed (R9 before the flip,
  R34 after); tear producers implement no unfenced subscribe; the daemon's
  `follow` treats an offset past its own as a gap (R21) instead of feeding it
  as contiguous, as it does today (S `held.rs:148-158`).
- **Effect:** duplicated output on attach mid-flood → 0, byte-exact against
  the journal; replayed events become invisible by construction (R34).
- **Gate:** 1,000 randomized interleavings of subscribe against numbered
  output reconstruct the journal with 0 duplicates and 0 gaps (red today); a
  `trybuild` case calling the two-call subscribe on a tear producer fails with
  E0599.
- **Old behaviour:** engate's two-call `subscribe()` + `snapshot()` remains
  for generic producers that cannot fence.

#### R43 · The authority parses what mado parses
*Destination · after R7, R15, R42 · a precondition of the event ring (R34) and
the flip (R38).*
- **Changes** (extends `PaneGrid`'s DCS hook, OSC dispatch and query answers
  (S `pane_grid.rs:1039-1061, 1130-1160, 1368-1404`), espelho's query catalog,
  `TearCaps`): *state parity* — OSC 4, 8, 10, 11, 12, 22, 104, 110–112 and
  1337 into the grid's state (palette, link table — SHUKEN's step 1 — colours,
  pointer shape), and BEL, OSC 9, 99 and 777 notifications and OSC 52
  clipboard writes into the event ring, where `PaneGrid` parses OSC
  0/1/2/7/133 only and BEL is a no-op today (S `pane_grid.rs:1075, 1368-1404`)
  while mado parses all of them (S mado `terminal.rs:6469-6487`); *answer
  parity* — DECRQSS, which tear's hook misroutes to its sixel buffer because
  it treats any final `q` as sixel (S `pane_grid.rs:1039-1042`), XTWINOPS 18t,
  the OSC 4/10/11/12 colour queries, the OSC 52 query, kitty's `CSI ? u` and
  DECRQM, each answered as mado answers it (S mado `terminal.rs:3555,
  3949-4039, 6235-6242, 6400-6435, 6510-6517, 6590-6653`). The catalog is an
  all-variants table in espelho: each row is a query mado answers today, with
  the bytes it sends, answered byte-identically by the host role or refused
  with a typed reason, and mado's own test fails when one of its query arms
  has no row. `DeclareTheme` and `DeclareCaps` — DA1's parameters, cell size
  in pixels, graphics and keyboard protocols — are negotiated session requests
  a viewer sends on attach and on change (R35 later carries them on the input
  lane); they feed the answers that describe a renderer, with a configured
  default when no viewer has declared; `TearCaps` derives from them, where its
  constant still says tear has no sixel (S tear-types `host_role.rs:68-83`).
- **Effect:** a pane answers every query mado answers today, whoever is
  attached, and the authority produces the palette, link table and events the
  view (R26) and the event ring (R34) carry; until R38 mado still answers what
  it displays (R42), so nothing changes for a watched pane.
- **Gate:** espelho parity over the catalog — every query mado answers today
  is answered exactly once by the host role, with the bytes mado sends (red
  today); a state row for every OSC above; BEL, OSC 9/99/777 and OSC 52 each
  yield one event; a catalog row removed fails mado's test.
- **Old behaviour:** none to keep: the authority gains state and answers, and
  who answers is R42's lease.

#### R21 · Gather at the source; a holder that serves many
*Destination · after R16, R19 · the holder half is held for the operator
(§8.2).*
- **Changes** (extends tamotsu's pump and single sink, tear-core's `pty.rs`
  reader and mado's local `pty.rs` — three serial copies today — and the
  PauseReader contract): one PTY source (working name `PtyPump`) on Ghostty's
  two-stage design (R: `Exec.zig`) — a non-blocking master, 4 × 64 KiB pooled
  buffers, trickles under 1 KiB delivered on the first EAGAIN, saturated
  streams bridged with ≤16 re-reads, then 1 ms polls inside a 3 ms budget,
  only while the consumer is busy; all buffers in flight means `PauseReader`,
  the only backpressure arm; batches stamped with `at`. The local half — the
  same source in tear-core and mado — has no holder dependency and lands
  first. The holder's single sink becomes a set with roles: one Authority, the
  daemon holding the store lease (R4), can slow the child through its bounded
  queue; Observers read lagging ranges from the journal and never block; a
  failing sink is shut down; replay is a lock-free cursor; a second Authority
  is told `Displaced`, and the higher incarnation wins (R4). An Authority that
  makes no dequeue progress for 2 s — not one whose queue is merely full,
  which a slow but healthy parser keeps full for a whole flood (`yes` parses
  at 9.6 MB/s, G) — is closed, and the holder runs journal-only until it
  re-attaches by offset; while it is away the journal pins eviction at the
  Authority's cursor up to a ceiling (`journal.pin_ceiling`, twice
  `max_bytes_per_pane` by default), and past the ceiling it evicts and records
  a typed `Gap{from, to}` that it reports on re-attach (holder capability
  `gap`), where today `read_from` clamps an evicted offset without a word (S
  `journal.rs:169-171`); a daemon that receives a gap resets the pane's
  parser, marks its history discontinuous and sends every consumer a keyframe.
  The recording gains a byte cap beside its 50,000-event cap (S
  `recording.rs:16, 85-101`), both enforced, or 64 KiB batches would raise its
  bound 64×. Holder capability `multi-sink`.
- **Effect:** batch size under a saturating flood 1,024 B → ≥32 KiB p50, ≥32×
  fewer writes, frames, sends and dispatches (F-b, H-d); the holder hop's
  journal-and-framing ceiling 407 → 1,130 MiB/s from 1 KiB to 64 KiB chunks
  (D); the Ctrl-C backlog bounded near 0.8 MiB (4 × 64 KiB batches plus socket
  buffers); replay never pauses the child.
- **Gate:** batch p50 ≥32 KiB and ≤32 frames per MiB under a saturating flood;
  held flood ≥0.8× the same run's `PaneGrid` replay rate (84.7–108.1 MiB/s at
  163 columns today, G); held and local warm echo within 1.25× of the previous
  release in the A/B (§6); an Observer stalled 10 s leaves the Authority's
  echo p90 within 1.25× of the same run's unstalled control and catches up
  with 0 B lost; a 128 MiB flood with the daemon stopped mid-flood and
  restarted ends byte-exact or with one typed `Gap`, never in silent
  continuation; no output pause >50 ms across a restart.
- **Old behaviour:** `pty.gather: per_read` (one batch per read, today's
  shape); PROTO-1 single-sink semantics for older daemons;
  `recording.max_events` keeps its meaning.

#### R22 · Encode once; bounded subscribers; no global lock on bytes
*Destination · after R9, R20, R21, R42, R43.*
- **Changes** (extends tear-core's subscriber map and `pane_callbacks` in
  `inproc.rs`, `recording.rs`, `registry.rs`, the daemon's
  `serve_subscription`): a per-pane `PanePipe` touches only its own state on
  the byte path; a per-pane ring of encode-once `Arc` frames with a byte
  budget (4 MiB) and a cursor writer per subscriber; a negotiated view
  consumer behind the ring head gets `Resync{at}` — a bounded keyframe, then
  frames. A byte subscriber, legacy or negotiated, is never closed for
  lagging, because an old mado on its non-switchable path reads a closed
  stream as the shell's exit, closes its window and kills its session (S mado
  `gui_tear_attach.rs:741-771, 1253-1268`): on a held pane it reads the
  lagging range from the journal by offset (makimono's `Journal::read_from`,
  the engine R37 later serves as the byte lane), so lag costs page-cache reads
  instead of daemon memory; on a process-bound pane it keeps a lossless
  backlog up to `subscriber.backlog_max_bytes` (16 MiB), past which it
  receives an in-band resync in `PaneBytes` — the skipped range's
  event-bearing sequences (BEL, OSC 9/99/777, OSC 52 sets) re-emitted from
  R43's ring, then the screen as VT with the complete `ModeSet` (R9), then
  live bytes — while the authority answers the skipped range's queries under
  R42's lease. A hang-up watcher prunes closed peers at once; recording export
  serializes outside every lock; a `PaneId` index replaces session scans.
- **Effect:** daemon CPU per extra subscriber +1.21 / +1.10 s per 128 MiB →
  ≤+0.05 s; one stalled consumer's growth 1.6 → 280.7 MB (M) → ≤4 MiB for a
  view consumer or a held pane's byte consumer and ≤16 MiB for a process-bound
  pane's; ghost subscribers, 1–2 per idle pane, → 0 (M); an export stalls no
  pane; per-key and per-tick lookups O(sessions) → O(1) (S).
- **Gate:** the fan-out, stalled-subscriber, attach-during-flood and
  export-during-flood rows; a 64 MiB flood ending in OSC 9 and a CPR, through
  a stalled legacy subscriber of a process-bound pane, yields one notification
  and one reply; an old mado's non-switchable window survives a stalled flood
  (a §6 compatibility cell); the `bench-probes` fault `subscriber.queue:
  unbounded` is red.
- **Old behaviour:** none to keep for the unbounded queue: 280.7 MB behind one
  stalled viewer is a bad state (M). Lossless byte consumers read the journal
  by offset.

#### R23 · Input that holds no shared lock
*Destination · after R21, R42.*
- **Changes** (extends `PaneIo`, `send_keys`, `pane_resize_absolute`, R42's
  reply writer and tamotsu's connection thread): a pane's PTY write half is
  owned by one writer — for held panes the holder's own input thread — and
  everything else holds a unit sender with no blocking write, where today a
  global PTY lock is held across the write and the resize (S
  `inproc.rs:1281-1314, 1363-1384`) and the holder writes on its connection
  thread (S `holder.rs:226-229`). The writer owns a FIFO of units — a key, a
  paste stream, a reply (R42) — over a non-blocking master (poll for
  writability and a wake pipe, ≤1 KiB pieces), so a resize, a cancel or a
  reply is serviced while a paste waits on a child that is not reading. A
  paste unit holds the writer until its end, its cancel, the loss of its link
  or a deadline, and on every abnormal end the writer itself writes ESC[201~;
  replies bypass the bound, never block the parser, and go out at the next
  unit boundary in query order; a full queue answers a typed
  `InputBackpressure`. A latest-value resize slot read between pieces gives
  one `TIOCSWINSZ` per burst and one reflow at the authority, in order with
  output; absolute resize updates the registry; `PtyHandle` drops on a reaper
  thread; `send_keys` acknowledges acceptance, not completion.
- **Effect:** a stuck paste blocks only its own pane, where today it blocks
  every pane's input, resize, subscribe, spawn and kill (S; one probe with a
  cooked-mode child did not reproduce it, M — the raw-mode case is the
  exposure); `SendKeys` to another pane stays near the uncontended RPC, 12–22
  µs p50–p99 for `get_pane` (H-a); a 60-step drag ≤2 SIGWINCH and ≤2 reflows;
  kill never sleeps on the caller (up to ~2.2 s in C1 today, G).
- **Gate:** with pane A holding a stuck 1 MiB paste into a raw-mode child that
  is not reading, `SendKeys` to pane B stays within 2× of the same run's
  uncontended p99 (≤200 µs on the reference Mac) and a resize to pane A lands
  within 2 ms; with an agent sending keys and the child sending CPR then DA1
  mid-paste, no foreign byte lands inside the brackets, replies leave in query
  order, and a forced disconnect closes the bracket; the drag and kill rows; a
  `trybuild` case writing through a unit sender fails with E0599.
- **Old behaviour:** `input.conflate_resize: false`.

#### R24 · A grid that allocates nothing
*Destination · after R7.*
- **Changes** (extends `pane_grid.rs`'s scroll, SGR and print paths and
  `blocks.rs`): rows recycled on scroll with an occupancy-limited reset, SGR
  parameters in a fixed array, an ASCII print path, OSC 133 blocks kept as
  offset and line ranges under the history budget, graphics evicted with their
  rows.
- **Effect:** allocations per KiB 513 / 70 / 15 (`yes` / coloured / text) → 0
  (G); `yes` and text throughput recorded as multiples of the same run's PTY
  ceiling, `yes` measured with the 3-byte lines a PTY delivers; the first
  reference-Mac run of the allocation-free grid sets those cells' multiples,
  because no measurement yet backs a target.
- **Gate:** a counting-allocator row (0 per KiB in steady state); espelho
  stays green; the `bench-probes` fault that allocates per row is red.
- **Old behaviour:** none to keep: the same output with fewer allocations.

#### R25 · Parser fast paths
*Destination · after R24.*
- **Changes** (extends vte 0.15 — offered upstream first, otherwise a vendored
  successor in tear): a bulk `print_ascii(&[u8])` fed by a word-at-a-time scan
  for printable ASCII, an SGR fast path and an `is_ground()` accessor.
- **Effect:** text at ≥2× the PTY ceiling (R: Ghostty devlog 006 reports 7.3×
  for SIMD ASCII, 2× on an ASCII `cat`, 1.4–2× for a CSI fast path);
  `is_ground()` lets checkpoints and resyncs fence on exact parser state.
- **Gate:** espelho parity between vte and the owned parser over the catalog
  plus a proptest; text at ≥2.0× the same run's ceiling.
- **Old behaviour:** `parser: vte`, while the owned parser earns parity.

#### R26 · History in bytes, frames by reference
*Destination · after R22, R24, R43.*
- **Changes** (extends tear-config's `ScrollbackConfig`, tear-core's
  `GridState.scrollback`, `snapshot` and `resize`, mado's `grid_damage.rs` and
  its byte-budget derivation): history becomes sealed 256 KiB `Arc` segments
  of UTF-8 text, line entries and style runs, with combining marks interned
  per segment and collected with it — where today the combining table only
  grows and refuses new marks past 65,535 entries without a word (S
  `pane_grid.rs:607-612`); segments are evicted whole once compact bytes
  exceed `scrollback.max_bytes`, declared today but unimplemented, like
  `reflow_on_resize` (S `tear-config` `lib.rs:309-325`); screen rows become
  `Arc<Row>`; after every parse batch the authority publishes an
  `OwnedPaneView` through `ArcSwap` — the amendment to SHUKEN §3 named in the
  status block — held during DEC 2026 with input modes ahead (§4.2); history
  is read by range; reflow re-lays logical lines when
  `scrollback.reflow_on_resize` is on, its declared default, where tear
  truncates today (S `pane_grid.rs:1729-1731`); `GridDamage` and `DirtyRegion`
  move into tear-types; the byte budget is derived once and shared with mado;
  embedded mado applies its own scrollback setting to its `InProcess`, rows
  and bytes, as `tear.impose` does for a daemon, where today it never bounds
  tear's grid (S mado `gui_tear_attach.rs:1425`). The daemon's default stays
  unlimited rows — the recorded "never lose anything" contract (S
  `tear-config` `lib.rs:238`) — now at about the size of the text.
- **Effect:** retained memory 2,639 B per 163-column row (G) → about the text
  plus ~12 B per line (estimate); 64 MiB printed 2.2 GiB, kept after the kill
  (H-d) → about the text, released on eviction or kill; a snapshot under the
  pane lock 1.07 / 19.5 / 272.7 ms at 10,363 / 104,062 / 1,047,598 rows (G) →
  a publish of 0.62–2.88 µs at 163×48 (4.79 µs at 212×58), independent of
  history by construction (C; depth not varied); MCP reads take no grid lock.
- **Gate:** with `max_bytes: 64 MiB`, RSS ≤128 MiB after a 64 MiB flood and
  back within 32 MiB of the baseline after the kill; publish ≤50 µs at 1M
  rows; reflow parity with mado on a corpus; ≤1.5 compact bytes per text byte
  on build logs; no frame published between BSU and ESU, and one published at
  the bound when no byte follows a BSU; a proptest past 65,535 distinct
  combining marks round-trips.
- **Old behaviour:** `scrollback.max_bytes: unlimited` (today's meaning, at
  ~1–1.5× the text instead of ~34×, H-d); `scrollback.reflow_on_resize: false`
  (today's truncation); `scrollback.rows` unchanged.

#### R27 · Restart costs the screen, not the journal
*Step (a) interim to (c); (b) held for the operator (§8.2); (c) destination ·
after R21, R22, R26, and (c) after R28; exact fences use R25's `is_ground()`.*
- **Changes** (extends `cmd_daemon`, `start_with_config`, the restore in
  `durable.rs`, tamotsu's readiness, the module trio): (a) bind and accept
  before restoring, sessions listed as restoring, panes restored concurrently
  on a bounded pool, config applied before restore, the file watcher built off
  the startup path — (c) later supersedes the bind ordering, while concurrent
  restore and config-before-restore stay; (b) a state-complete checkpoint
  written at a feeder-quiescent offset every 4 MiB of output and forced at 8
  MiB, re-adoption as checkpoint + `Attach{from: checkpoint.at}`, deep history
  backfilled from the journal in the background, and held-spawn readiness on
  an inherited pipe instead of 10 ms polls. The checkpoint is a second
  serialisation of the grid, which SESSION-DURABILITY §3.3 chose against ("raw
  bytes, not rendered cells"), so it waits for the operator; it must carry
  what `to_ansi` drops today — graphics, title, cwd, OSC 133 blocks, DECSTBM,
  the palette and the primary screen under an alternate one (S
  `pane_snapshot.rs:372-480`) — or that pane re-adopts from offset 0. (c)
  socket activation (launchd `Sockets`, a systemd `.socket`), so the socket is
  never unbound across a restart, through R28's seam crate, whose one
  `#[allow(unsafe_code)]` module gains `launch_activate_socket` and
  `LISTEN_FDS` adoption, so tear's crates keep `forbid(unsafe_code)`.
- **Effect:** time to accept — up to 540–547 ms at the last two logged starts,
  while the file watcher was built before the accept thread, and 3–6 ms at
  earlier ones (S, daemon log) → ≤10 ms at every start whatever the pane
  count; a pane's first correct frame after a restart, a full journal re-parse
  of 0.59–0.71 s per 64 MiB with the child paused (G), → a checkpoint plus a
  tail of at most one checkpoint interval (4 MiB: 46–47 ms of parsing at
  today's rate, G, one run each); held `NewSession` 23.8 → ≤10 ms (H; the exec
  is 3.8 ms, D).
- **Gate:** 20 panes with 64 MiB journals: accept ≤3× the same run's spawn +
  `openpty`; each pane's restore — checkpoint load, tail parse, first frame —
  ≤1.25× the same run's replay of its tail, and the last of the 20 ready
  within ⌈20 / pool size⌉ × that; checkpoint + tail and offset-0 replay
  produce espelho-equal grids over a corpus with graphics, title, OSC 133,
  DECSTBM and palette changes; no output pause >50 ms; held `NewSession` p50
  ≤5× bound `NewSession`; 0 refused connections across `launchctl kickstart
  -k`, and across a `bootout`/`bootstrap` of a changed plist the refused
  window is measured and every client re-dials (R3).
- **Old behaviour:** `sessions.restore: before_bind`; `checkpoint: off`
  (offset-0 replay, which older daemons keep doing: checkpoints are new files
  they ignore).

#### R28 · Threads that say what they are
*Destination · after R2, R21.*
- **Changes** (extends kanshou's state tree, in the single-FFI-seam shape of
  `nawabari-xnu`, and the seam crate R2 introduced): the Darwin scheduling
  seam crate — its name to be minted through the naming law, proposed and not
  ratified — holds one `#[allow(unsafe_code)]` module around
  `pthread_set_qos_class_self_np`, `posix_spawnattr_set_qos_class_np`,
  `setpriority(PRIO_DARWIN_PROCESS)` and `proc_pidinfo`, a typed `Absent`
  elsewhere, and exposes `ThreadClass{Interactive, Utility, Background}` and
  `spawn_classed`; gather, parse, input, subscriber and connection threads are
  Interactive (user-initiated), the syncer, persister, document writer and
  checkpoint are Utility, backfill is Background; clippy refuses raw thread
  spawns in tear crates. The holder resets its main thread to the default
  class before spawning the shell and clears inherited background state on the
  child before exec, because a holder spawned from a user-initiated connection
  thread may inherit that class (unmeasured, §8.3) and a process-wide
  background clamp overrides any thread class (F, R); kanshou gains
  `process.band {declared, observed}` and a per-pane pipeline leaf.
  tear-bench's floors and mado's link threads (R13) use the same seam.
- **Effect:** read stages run at user-initiated QoS — Ghostty reports a 15 %
  throughput difference from this change on an M4 Max and says it is not 15 %
  in total (R: `Exec.zig`); shells run at PRI 31 whatever their daemon's
  class; a band regression becomes a typed reading instead of a surprise.
- **Gate:** a `proc_pidinfo` row at XNU's QoS priorities (R): gather, parse
  and input ≥37, the syncer 20, the shell 31, the shell spawned through the
  production `NewSession` RPC with the daemon at `session-host` and at
  `xpc-adaptive`; flood no lower than R21's in the same run, the delta
  recorded until tear-bench backs a target; clippy's disallowed methods pass.
- **Old behaviour:** `threads.qos: off`.

### Phase F — the lanes and the flip

#### R29 · Lane sessions
*Destination · after R15, R19.*
- **Changes** (extends `Capability`, `Hello`, the daemon's accept loops and
  `shutai`): `MintLanes{kinds}` on an authenticated session returns single-use
  tickets (128-bit; bound to client id, identity and lane kind; 10 s to live);
  every lane's first frame is `LaneHello{ticket}`; the unverified-lane type
  has no serve method; new capability rows `lanes-v1`, `view-lane`,
  `input-lane`, `bulk-lane`, `byte-lane`, `control-ids`, `registry-feed`,
  `view-graphics` and `remote-deflate`; lane clients are constructible only
  from a `Negotiated<Cap>` witness.
- **Effect:** every lane authenticates the same way over UDS, ssh-forwarded
  sockets, TCP and WebSocket; SHUKEN §5's row for a capability-gated field
  read on a daemon that never advertised it moves from only-mitigated to
  parse-time-rejected.
- **Gate:** each lane × {no ticket, expired, reused, another client's} is
  refused, typed, before any payload.
- **Old behaviour:** `lanes.<kind>.enabled: false` stops advertising that lane
  and refuses tickets for it alone.

#### R30 · mado reads one frame — SHUKEN step 2
*Destination for the readers · after R13.*
- **Changes** (extends mado's `Grid` rows, the renderer's `snapshot()`,
  `ux/engine.rs` and GRID-THREADING-CONTRACT's `Arc<Line>` groundwork): rows
  become `Arc<Line>` with a row id and a version; the view lane owns mado's
  `Terminal` (`pub(in crate::view_lane)`) and publishes *mado's projection* —
  an immutable frame of the same shape as tear's `OwnedPaneView` — when the
  next read would block, at most every 4 ms under a flood, held during DEC
  2026 with input modes ahead; a mado trait over resolved rows and cells is
  implemented for this projection now and for tear's `OwnedPaneView` at R38,
  where the projection goes and the trait collapses into the one type; in one
  commit `render.rs` (25 `term.*` calls behind 4 lock sites) and
  `ux/engine.rs` (87 calls behind 36 lock sites: 70 through a `term` binding,
  17 chained on the guard) move to the frame plus a mado-owned `Viewport`,
  along the SHUKEN §5-B border. This keeps SHUKEN §5-C: every reader moves in
  one commit onto one model, produced by the one parser those readers already
  used; at R38 only the producer changes.
- **Effect:** per painted frame, a copy of every visible row under the
  terminal read lock (S `render.rs:3854-3919`), then a URL scan of that copy
  and a clone of every search match under the search mutex (S
  `render.rs:3926-3937`) → one `Arc` load and a version diff; mado's MCP reads
  the live frame in every runtime, where today it is blind in the resident one
  (M).
- **Gate:** naming `Terminal` from any UI module is E0603 (a compile-fail
  test); render goldens and the 32-frame determinism test stay byte-identical;
  the engine suite stays green; the `bench-probes` reader that clones rows is
  red.
- **Old behaviour:** none for the readers (one commit);
  `render.sync_output_max_hold_ms` for the hold.

#### R31 · A renderer that redraws what changed
*Destination · after R30.*
- **Changes** (extends garasu's `ShapeCache`, `QuadPipeline` and surface
  configuration, mado's render passes and `grid_damage.rs`): a garasu
  `CellGridRenderer` and glyph atlas keep GPU slots per screen row keyed by
  (row id, version); a per-frame row → slot table read by the vertex shader
  turns scrolling into re-pointing; only missing or changed rows are rebuilt;
  uploads go through one `StagingBelt`, already in wgpu 25.0.2; the grid draws
  in one pass (clear, backgrounds, images below, glyphs, images above, then
  cursor, selection, search and URL layers); URL detection is cached per row
  version; search matches swap as one `Arc<[Match]>` behind a search epoch;
  glyphon stays for overlays, which keep their prepared text while unchanged.
  mado's private LRU shape cache and its overlay cache move onto garasu's
  `ShapeCache`, which was promoted there because mado's was being copied (S
  garasu `shape_cache.rs:1-18`; mado `render.rs:1363-1374, 1776`), extended
  with font features and synthetic italic; garasu's surface configuration
  (Fifo, latency 2, S garasu `context.rs:263-292`) and madori's (AutoVsync,
  latency 1, S madori `app.rs:886-938`) become one typed present policy in
  garasu that madori consumes (R14).
- **Effect:** a one-row change, never measured in isolation (L has two
  last-frame readings of unknown content, 886 and 3,634 µs; R1's paint
  histograms record the baseline first) → ≤0.2× the same run's full rebuild of
  that screen at p50, an estimate the gate checks; staging Metal buffers ≥3
  per painted frame, 5 with an overlay (S), at 7.1–7.2 µs each (F-gpu) → 0
  after warm-up; ≥3 full-surface passes → 1 (S); the Ctrl-S frame — ~4,900 µs
  in a 2026-08-21 measurement (S `render.rs:6418-6423`) that predates
  `aa031a2`'s overlay shape cache, unmeasured since, so R1 sets its baseline —
  → ≤1 ms; an idle open search bar 60 → 0 frames/s (S `render.rs:6381`). The
  compositor's cost is unchanged by construction.
- **Gate:** a headless bench (one-row change p50 ≤0.2× the same run's full
  rebuild; full redraw no worse than the previous release in the A/B); pixel
  equality with the full-rebuild path over randomized edit and scroll scripts;
  staging chunks flat over 1,000 frames; one pass with effects off.
- **Old behaviour:** `render.grid_renderer: full_rebuild`.

#### R32 · Attach, switch and reattach are swaps
*Destination · after R9, R13, R30.*
- **Changes** (extends mado's switch, reattach and boot paths, and engate's
  typestate on the lane thread): one `ActivePane{pane, view, epoch}` value;
  input carries its target pane when enqueued; `prepare_attach(pane, size)`
  subscribes size-first, builds the view off the UI thread — before the flip
  an R9 keyframe into a fresh mirror with history backfilled behind it, after
  it R34's keyframe — and posts `Prepared`; the UI swaps `ActivePane` in one
  assignment; the old lane is torn down off the UI thread; a failure posts
  `SwitchFailed`, and screen and input stay where they were; reattach starts
  from the stream's end with capped backoff (500 ms, doubling to 60 s) and a
  typed link state, resuming at the last applied offset (R20); startup
  prepares the session while the window and GPU initialize.
- **Effect:** UI time per switch 154–162 ms at 256 KiB of history to 2.3 s at
  4 MiB, dead at 8 MiB (H-f) → ≤1 ms; a failed switch no longer sends keys to
  one pane while another is shown, nor retries every 500 ms with two full
  encodes each (S); the first frame comes at max(GPU init, tear setup) instead
  of their sum, unmeasured today.
- **Gate:** ≤1 ms of UI dispatch while switching to a pane with 4 MiB of
  history; failure injection keeps screen and input on the old pane (red
  today: `gui_tear_attach.rs:1074-1079` re-points first); the backoff row; the
  startup phase log shows the event loop running before the session exists.
- **Old behaviour:** `tear.reattach_backoff`; switching on the UI thread is
  R13's bad state.

#### R33 · Resize is a latest value; the authority reflows
*Destination · after R13; the reflow moves to the authority after R26.*
- **Changes** (extends mado's `push_grid` and `rewrap_to_cols`, the link's
  resize slot, R23's slot at the authority): a window resize updates surface
  and `Viewport` at once and puts columns × rows in a latest-value slot — one
  request in flight, newer sizes replacing the pending one — sent after
  `window.resize_coalesce_ms` (50; R: Ghostty 25, foot 100) and on release;
  until the reflowed view arrives the current one is drawn anchored; before
  the flip the mirror reflows on the view lane, after it the authority does;
  with several windows on one pane only the size owner resizes (daemon policy,
  latest-focused by default) and the others letterbox.
- **Effect:** per drag step, an O(scrollback) rewrap under the write lock, a
  blocking RPC, a truncating resize and a SIGWINCH (S) → a surface reconfigure
  and one frame; one reflow and one SIGWINCH per settled size.
- **Gate:** a scripted 60-step drag: ≤1 ms of UI per step, ≤1 request per
  coalescing window plus a final one, authority reflows equal to settled
  sizes, ≤2 SIGWINCH in the child; a two-window size ping-pong row.
- **Old behaviour:** `window.resize_coalesce_ms: 0`; `tear.size_policy:
  manual`.

#### R34 · The view lane on the wire
*Destination · after R20, R26, R29, R30, R43.*
- **Changes** (extends R26's publisher, R9's codec and a client replica): the
  authority sends each consumer the difference between the last frame it was
  sent and the newest — rows whose `Arc` differs, plus rows that left the
  screen; at most 2 unrendered deltas per (consumer, pane), returned by render
  acks; observers ack on receipt under a rate cap (MCP 1 Hz, picker previews
  10 Hz); publish-and-send runs inline after a parse batch when credit is
  free, so the first change after idle leaves at once; style and link tables
  ship incrementally under a bound, after mado's style-table precedent; the
  replica applies off the UI thread and refuses a delta whose base is not its
  epoch, with a typed resync; the event ring — bell, notifications, OSC 52
  writes, clipboard reads as a 1 s default-deny upcall, pane end — delivers at
  once and never before the attach fence; side-effect events are deduplicated
  per process by (pane, offset), so two windows of one mado post one
  notification; a graphic a delta references stays pinned until that
  consumer's render ack, so an animation that transmits a frame and deletes
  the last still renders; a disconnected consumer's state is kept 60 s so it
  can resume with a delta.
- **Effect:** typing deltas 74.8 B on average, a steady build log 344 B a
  frame, a full-screen flood 3,968 B a frame on average and 4,277 B at most —
  ~476–513 KB/s per consumer at 120 Hz for build-log content whatever the
  output rate (C), against 1.98× the output rate today (H); per-cell truecolor
  reaches 87,351 B a keyframe (C), ~10.5 MB/s at 120 Hz, until §8.3's
  inline-style fallback bounds it; idle lanes are silent; a stalled consumer
  costs one frame, not 280.7 MB (M); pane-fate polling is gone for good.
- **Gate:** the codec proptest; view-attach time flat across 0, 1k, 10k and
  100k rows (≤1 ms); ≤2 deltas per consumer frame through a flood; RSS growth
  ≤ one frame per consumer stalled 10 s through 64 MiB; events exactly once
  across a clean handoff and at most once across a daemon crash, and a
  notification emitted while the daemon is down is either delivered once after
  re-adoption or counted as lost, never twice; no frame inside a split BSU,
  and the timeout at the bound ±5 ms with no bytes after the BSU; 0 lane
  messages over 60 idle seconds.
- **Old behaviour:** consumers that open no view lane keep the byte path;
  `lanes.view.{credits, observer_rate_hz, resume_grace_s}`.

#### R35 · The input lane
*Destination · after R23, R29, R43; exactly-once needs R16 holders.*
- **Changes** (extends R23's per-pane writer, tamotsu's control verbs and
  mado's sinks): `InputMsg{pane, serial, epoch, bytes}`, fire-and-forget, with
  keys and mouse reports encoded at the renderer under the input modes of the
  newest frame, which arrive ahead of any held cells (§4.2); a key encoded
  under input modes older than the authority's — DECCKM, DECKPAM, kitty flags
  or mouse encoding changed after its epoch — is refused `KeyModeSkew`, that
  key alone, and re-encoded under the newer modes; paste streams as
  `PasteBegin{id, bracketed_under}`, chunks, end and cancel under a 64 KiB
  credit window into R23's paste unit, and a paste framed under a stale
  bracketed-paste mode is refused `PasteModeSkew` — that paste alone — and
  re-framed; `ViewportDeclared`, `ThemeDeclared` and `CapsDeclared` are
  latest-value, carrying R43's declarations on the lane; mouse motion is one
  report per frame; acks are cumulative; refusals are typed; dedup by (client
  id, pane, serial) at the PTY writer, through holder capability
  `write-serial`.
- **Effect:** ~1.2–2.3 µs of UI per key (F-e sender side); 0 keys lost or
  doubled across reconnects; pastes of any size, cancellable within one ≤1 KiB
  PTY write for raw-mode children (`TTYHOG`, G); hovering costs ≤1 message per
  frame and 0 RPCs.
- **Gate:** key loss with a forced reconnect every 100 keys: 0 lost, 0
  doubled; a 64 MiB paste byte-exact, drained at ≥0.8× the same run's raw-mode
  paste floor, with ≤1 ms of UI time and cancellable within one write; a
  mode-skew row for keys inside a synchronized update and one for paste; ≤10
  µs of UI per key at p99.
- **Old behaviour:** lockstep `SendKeys` stays served; `lanes.input.*`.

#### R36 · The control lane and the registry feed
*Destination · after R29.*
- **Changes** (extends `wire.rs` — whose push `Notification` type is already
  promised (S `wire.rs:344-346`) — the daemon's serve loop, tear-client's
  `rpc` and mado's picker): `Tagged{id, deadline_ms, req}` requests answered
  out of order by workers dispatched by class, except that requests that
  affect a pane's input — `SendKeys`, resize — stay in order per (client,
  pane); an expired request is answered `Expired` without doing the work;
  tear-client gains `Pending<T>` and blocking wrappers with a 2 s default
  deadline; a registry feed pushes session, window, pane and layout changes
  with generations, and a mutation carrying a stale `expected_gen` fails
  `Conflict`.
- **Effect:** a 23.8–218.6 ms `NewSession` (H) no longer delays a concurrent
  call; the picker opens from a local replica with 0 RPCs.
- **Gate:** with a held `NewSession` in flight, `get_pane` p99 <1 ms; opening
  the picker over 50 sessions makes 0 blocking calls; a layout compare-and-set
  conflict row; a key and a resize sent back to back by one client arrive in
  order.
- **Old behaviour:** untagged lockstep requests stay served;
  `lanes.control.default_deadline_ms`.

#### R37 · The bulk and byte lanes
*Destination · after R9, R20, R21, R22, R29.*
- **Changes** (extends R9's requests, makimono's `Journal::read_from`,
  `recording.rs` and `serve_subscription`): bulk — R9's `Keyframe` and
  `HistoryRange` move onto the bulk lane, with graphics by id (pane plus the
  offset of transmission, stable across restarts), one-shot views for
  observers, `Search{pattern, range}` returning matches by stable line id
  (R38), streamed exports and cancel; bytes — `OpenBytes{from: offset |
  keyframe | live, mode: read-only | interactive}` with raw `DATA{at}` frames
  coalesced to 64 KiB, held panes read from the journal by offset so a lagging
  reader never touches the pump (the engine R22 already uses), other panes
  from a bounded ring with `Gap` then a VT keyframe; the host role follows
  from the attached lanes; the legacy subscribe is served by the same engine;
  recording reads a cursor.
- **Effect:** byte consumers at the PTY ceiling, up from 105 MiB/s (W); no
  transfer anywhere is O(history), because no request returns more than a
  page; graphics move by id and are never cloned into snapshots.
- **Gate:** 100k rows of scrollback paged with no frame >64 KiB; MCP
  `pane_snapshot_text` ≤1 ms at 0, 1k and 100k rows × 300 columns; byte-lane
  flood ≥0.85× the same run's PTY ceiling at PRI 31 (an absolute 200 MB/s
  would sit at 0.86–0.96× of the ceiling's 209–233 MB/s, inside the noise,
  H-d, F-b); offset resume across a reconnect byte-exact; a lagging reader
  gets `Gap` and a keyframe with bounded RSS; a second interactive relay is
  refused.
- **Old behaviour:** `lanes.bytes.{enabled, legacy_subscribe, lag: resync |
  disconnect, ring_bytes}`.

#### R38 · The flip — SHUKEN steps 1, 3 and 4
*Destination · after R5, R26, R30, R34, R42, R43.*
- **Changes** (extends SHUKEN's own sequence): in tear, the preconditions this
  pass found beyond SHUKEN §5-C's list are in place — R5's modes, R26's
  reflow, R42's lease and reply writer, R43's state and answer parity, where
  the OSC 8 link table (step 1) lands — and `ModeSet` is sealed: private
  fields, a crate-private constructor in tear-types, no `Default`, decoded
  only inside `OwnedPaneView`, where today it is pub-field, `Default` and
  `Deserialize` and a separate fetch exists (S `modes.rs:124-135`,
  `control.rs:311-327`). In mado, the view-lane producer becomes the authority
  — `InProcess::subscribe_view(pane, waker)` in C1, R34's replica in C2–C6;
  mado declares `answers_queries: false`, so the authority answers for every
  pane — one declaration, no commit that spans two repositories; the mirror
  producer and the response writer are deleted; the seven mode accessors are
  deleted (step 3); the local-PTY fallback becomes an embedded tear pane; vte
  leaves the manifest (step 4); R30's trait collapses into `OwnedPaneView`.
  Search, copy, URL detection in scrollback and prompt marks read the replica,
  which keeps rows by stable line id and backfills holes through bulk pages
  before a search runs; `Search{pattern, range}` (R37) returns matches by line
  id, a pure function of the view, while search state stays renderer-side per
  §5-B.
- **Effect:** parses per byte 2 → 1 in C1 and 1 + N → 1 in C5; embedded attach
  1.3–39.5 ms (H-f) → an `Arc` load; attach after 8 MiB of history, dead today
  (H-f), → first correct frame ≤20 ms.
- **Gate:** a manifest test (no vte in mado); mado's parsed-bytes counter at 0
  through a 64 MiB flood; espelho and Gate-A parity (wide, combining, reflow,
  kitty flags) green; R43's catalog answered exactly once with mado attached;
  search, copy and marks parity before and after the flip over a corpus that
  includes a flood; a `trybuild` case constructing a `ModeSet` outside
  tear-types fails with E0451; SHUKEN §5's rows re-graded with red runs.
- **Old behaviour:** against a daemon without `view-lane`, mado degrades per
  pane to doorbell-driven reads of the authority's own snapshot — R9's
  `Keyframe` where the daemon offers `snapshot-range`, else the legacy
  `PaneSnapshot` — converted to an `OwnedPaneView` inside tear-types, the only
  mint outside the authority and only from the authority's own snapshot; a
  pane whose snapshot exceeds the frame cap is refused, typed, for that pane
  alone, naming the remedy. The embedded runtime stays available. No local
  parser comes back: that is the decided destination.

#### R39 · Remote carriers
*Destination · after R34–R37.*
- **Changes** (extends tear-client's `Transport`, the daemon's TCP accept —
  `--tcp` becomes a listener of the durable daemon instead of a separate
  flavour (S `main.rs:1896-1904`) — tear-ws-bridge, mado-web and mado's input
  engine): the same lanes over TCP, ssh-forwarded sockets (one channel per
  lane) and WebSockets (one per lane, forwarded without copying); stream
  deflate (raw deflate, level 1, one sync flush per frame, through
  `miniz_oxide`, pure Rust and already in mado's lock) only off-host and never
  under ssh compression; delivery clocked by acks, at most 2 deltas per round
  trip, which is mosh's SRTT/2 without an estimator (R); predictive echo as a
  renderer overlay, on above 30 ms of smoothed round trip and off below 20 ms
  (R: mosh); a reconnect resumes by (client id, last frame, event cursor,
  input serial) within 60 s; the bridge accepts blocking, woken by a
  self-connect on stop (R8's shape), and parks its main thread on the stop
  signal, where today it polls every 50 and 200 ms (S tear-ws-bridge
  `lib.rs:136-138`, `main.rs:50-52`); one writer owns each WebSocket's stream
  and pongs queue into it, where today two state machines write one
  `TcpStream` (S); mado-web decodes deltas in wasm, and because it pins the
  published tear-types (S mado-web `Cargo.toml:21`) it runs beside §6's
  compatibility matrix.
- **Effect:** bytes per typed key 469,852 → ~28 B of compressed delta plus ~10
  B of input; compressed frames of 161 B for a build log, 478 B for a TUI
  redraw and 1,525 B for a full flood (C); TCP connect <1 ms and subscribe <2
  ms: the UDS path's 0.29 + 0.25 ms (H) plus ~10 µs per extra TCP round trip
  (F-a); the bridge's 25 timer wakeups a second (S) → 0.
- **Gate:** a delay proxy at 0, 20, 50, 100 and 250 ms: ≤2 deltas per round
  trip, bytes per frame within 10 % of the prototype, prediction on above 30
  ms, resume within grace with no keyframe and 0 input lost or doubled; an
  ssh-against-TCP echo row, the Nagle probe; an idle bridge makes 0 wakeups
  over 60 s.
- **Old behaviour:** `lanes.remote.{compression: auto | off | deflate,
  prediction: auto | off | always}`; the standalone `--tcp` daemon.

#### R40 · Observers on lanes
*Destination · after R36, R37.*
- **Changes** (extends tear's `mcp.rs`, mado's MCP, kanshou's discovery and
  forwarding, and the picker): tear's MCP keeps one connection and reads
  one-shot views and pages, reports the right daemon pid and renders blind as
  blind; `daemon_status` takes cpu, uptime and bytes consumed from the
  daemon's kanshou process leaf or renders them blind, where today it reports
  a single-sample cpu that reads 0, the MCP process's own uptime and a
  hard-coded 0 as found (S tear `mcp.rs:78-160`); mado's MCP reads resident
  sessions through the window's replica; picker previews are rate-capped
  watches; discovery is cached; kanshou's forward reports an answer over its 4
  MiB cap as a typed `TooLarge` — refused, never "no live GUI reachable",
  which is what a large answer reads as today (S kanshou `client.rs:193`,
  `mcp.rs:36-49`); the embedded snapshot leaf serves text rows by default and
  cells on request, where today it returns every visible cell as ~95 B of JSON
  (S).
- **Effect:** per MCP read, connect + Hello + N+1 RPCs → ≤2× one RPC;
  `daemon_status` 3.3 ms → one RPC (M); reads never fail on depth or size.
- **Gate:** the C13 cells; on a resident window, mado's `list_sessions` counts
  ≥1; an introspection answer over 4 MiB is refused `TooLarge`.
- **Old behaviour:** the one-shot connect stays for CLI use.

#### R41 · Seal the UI at a crate boundary (optional)
*Destination, a tier upgrade · after R38.*
- **Changes:** the UI — render, ux, picker and adapters — moves into a crate
  whose manifest lacks tear-client and tear-core.
- **Effect:** "the UI thread blocks on tear" moves from a structural test to
  truly-unrepresentable for accidental regrowth (E0433), the grading SHUKEN §4
  gives its vte seal.
- **Gate:** a manifest test and a compile-fail test.
- **Old behaviour:** none; a move.

## 6. The all-variants gate

**Where.** `tear-bench`, a tear workspace member (`publish = false`, a library
and the `tearbench` binary), ports the 2026-10-07 harness and floor suite with
every machine-specific value turned into a flag.
mado's present-path half lives in mado's benches and structural tests and
invokes the same declaration.

**The declaration.** One `bench_matrix!` invocation. A row is a case, its
per-metric budgets as a struct literal — a missing metric is E0063, and
`Budgets` deliberately has no `Default` — a mandatory receipt string and its
negative controls. `Case` is C1–C13 with typed sub-variants (`C6{Tcp, Ssh,
WsBridge}`, `C7{Attach, Switch, Reattach, Readopt}`, `C12{Keys, Paste,
Mouse}`). `Variant` is runtime × `Durability` × transport × band ×
`JournalSync`, mapped from the product's own enums by exhaustive matches.
tear's `Durability` and `HostRole` and tear-client's `Transport` are not
`non_exhaustive` (S) and are matched in tear-bench, so a variant that lands
without a row is E0004 in tear's workspace build; mado's `TearRuntime` and
madori's `FramePacing` live in repositories that depend on tear, not the
reverse, so they are matched in mado's bench crate and a new variant there
goes red when mado builds against it — for madori, at mado's next lock bump. A
budget is `Floor{floor, stat, k}`, `Count{max}`, `Bytes{max}`,
`NotApplicable{why}` or `Pending{rung, today, receipt}`; a `Pending` that
names a landed rung fails a generated const assert.

**Why Rust and not tatara-lisp.** The ladder puts declarations above Rust, and
budgets are declarations. But the proof this gate exists for — *a variant
cannot land without a row* — is rustc's exhaustiveness over the product's Rust
enums, and a parsed declaration would demote it to a CI check. That is the
ladder's own exception: neither rung above Rust can express it. Results are
data, one TSV row per sample, queried with duckdb; a one-way tatara-lisp
projection can be generated if the typescape wants the matrix, never a second
authority.

**Floors, in the same run.** Every timing budget is `stat(case) ≤ k ×
stat(floor)`, the floor measured in the same run by an independent code path:
UDS round trips (raw and framed), UDS one-way sends, PTY echo, wakes (hot,
from idle, through the run loop), PTY output and input ceilings, UDS
throughput, the flush primitives, serialization and timers, plus four the
2026-10-07 pass lacked — spawn + `openpty` (C8), screen parse (C7), present
(mado), and a parked madori window with no tear link (C10). Their FFI sits
behind one seam with exactly one `#[allow(unsafe_code)]`, shared with R28. A
rung's gate may state its acceptance on the reference Mac in absolute units
where that reads more plainly; the matrix encodes each such value as
`Floor{floor, stat, k}`, with `k` the value divided by its floor measured on
the reference Mac, so a GitHub VM and the reference Mac grade the same
multiple and no budget holds a microsecond literal.

**Red on today's code.** A timing cell counts as a rung's evidence only when
today's code is red in every recorded run; a cell today's code already meets
in any run guards against regression and nothing more. The echo after a 2 ms
gap is report-only: its run-to-run spread, 2.39× (H), is as wide as any
multiple it could gate.

**Negative controls.** Every budgeted cell has at least one: a good state
today's behaviour exhibits, kept as configuration (each rung's *Old
behaviour*), or, where today's behaviour is a bad state, a fault injector
compiled only under `bench-probes` and never a configuration value — R6's
test-only array encoder, the mute sink (R4), the lease turned off (R42), the
wake turned off (R10), `ui_io: inline` (R13), `snapshot.history: all` (R9), an
unbounded subscriber queue (R22), an allocating row path (R24) and a reader
that clones rows (R30). Every CI run switches each control on and asserts its
red set: exactly, for count cells; for timing cells, as a minimum effect
beyond the noise band below. A control that reddens nothing, or the wrong
cells, fails. The gate proves on every run that it can see its own regression.

**Verdicts are derived, never declared.** A cell is `Within`, `Over`,
`Pending`, `Blind`, `Errored` or `NotApplicable`, with its sample count. It is
`Blind` when a precondition fails: four sentinels — a framed UDS round trip, a
channel wake, a UDS one-way send and a run-loop wake, p50 — within 1.25× of
the host class's reference, the last two added because they drifted 1.35× and
1.39× between the floor suite's passes while the first two held within 1.01×
and 1.15× (F); the daemon's version and capabilities match the variant;
daemon, holder and shell run in the declared band; enough samples. Zero
samples is `Errored`. Three consecutive `Blind` runs on one host class are
red, because *cannot measure* is a defect, never a pass. Summaries render with
kotae's four outcomes, so blind never reads as found.

| Tier | What | Where | When |
|---|---|---|---|
| structural | count metrics (RPCs, bytes, frames, flushes, parses, encodes, allocations per KiB, idle ticks, paints per change), negative controls, `trybuild` proofs, the ledger test | tear's existing test gate on Linux with `--all-features`, and `ci.yml` on pull requests | every push |
| timing | floor-relative latency and throughput, A/B against the previous release in interleaved ABBA blocks on one VM; a regression is a bootstrap 95 % lower bound of head/base p50 above 1.10, retried once | pleme-io/actions' `benchmark-runner`, extended rather than forked: its body becomes a `run.tlisp` that runs `tearbench gate --tier timing` and fails on the matrix's verdicts, where today it runs `cargo bench` under `\|\| true` and reports 0 regressions whatever happens (S actions `benchmark-runner/action.yml`); never on pull requests | every push to `main` and nightly; report-only for a new host class's first two weeks |
| reference | every tier | `nix run .#bench -- gate --tier all` on the reference Mac | at every rung boundary; the receipt and the red run go in the commit message |

**Noise, as measured.** Between the floor suite's two passes (load average
4–30), interactive-band p50 floors reproduced within 1.01–1.42× — framed round
trip 1.01×, channel wake 1.15×, UDS one-way 1.35×, run-loop wake 1.39×, PTY
echo 1.42× — background-band p50s within 1.75× and their p99s within 5.3× (F).
At PRI 31 the warm echo reproduced within 1.01× and the mado-shaped key within
1.007×; in the deployed band, within 1.25× and 1.12×; an echo after a 2 ms gap
within 2.39×; maxima only within 22× (H). So p50 and p90 are gated; p99 only
with multiples at least 3× looser and quiet sentinels; maxima are reported and
never gated; a count is used wherever one exists.

**Compatibility matrix.** `DaemonPeer{Prev, Head}` × `ClientPeer{Prev, Head}`
× `HolderPeer{Oldest = v0.1.28, Prev, Head}`: 12 cells on every timing run,
`Prev` built by nix from the previous tag at run time. Each cell checks Hello
and capability sets, 20 of 20 keys (after an oversized snapshot attempt too),
byte-identical echo, a first attach frame equal to the authority's text and
modes, a subscription that survives 8 MiB of history, a 1 MiB paste, a resize,
re-adoption of a held session, session documents written by `Head` read by
`Prev`, an impose from the client that leaves every key newer than its schema
unchanged, a muted holder that recovers, an old window that survives a stalled
flood, and exactly one answer per query. Beside the cells, the bridge path
runs mado-web built against the published tear-types.

**The ledger is enforced.** §8.1 sits under a `<!-- tier-ledger -->` marker,
the shape SHUKEN's ledger already uses, so skill-lint's tier-ledger check —
selo's vocabulary, with every only-mitigated row naming its ceiling —
validates it in tear's CI once that check runs over tear's docs. The check grows once, in
skill-lint, instead of a second parser being ported from mado's
`unrep_ledger.rs`: every row must carry an executable red run (a negative
control, a fault injector, a compile-fail or evaluation test, or a mutation
id), and every truly-unrep row a compile-fail or evaluation test with pinned
output. Seam code — verdicts, noise, the matrix and capability macros,
tamotsu's protocol — runs under the fleet's cargo-mutants gate.

## 7. Compatibility rules

Version skew here is permanent, not transitional: a resident daemon outlives
mado upgrades; holders outlive daemon upgrades by design; a rollback pairs an
older daemon with state a newer one wrote; C6 adds skew across hosts.

1. **Names, never integers, decide behaviour, in both directions.** The daemon
   advertises capabilities (exists today); clients declare what they decode,
   in `Hello.client_capabilities` and per subscription in
   `SubscribeWith.accepts`; holders and daemons exchange names in both Hello
   frames. `PROTO` stays 1 under a const assert; `DOC_VERSION` stays 1 for
   additive fields.
2. **The wire only grows.** New fields are `serde(default)`, new variants are
   appended, nothing is renamed, reordered or removed, and nothing denies
   unknown fields. A new request is sent only after `require(cap)`; a new
   pushed frame can be written only to a `NegotiatedSink` whose peer accepted
   it, and `LegacySink` has no method for it (E0599). Gate 0 lists a dropped
   field as a silent discard (class 6); here a field whose loss would change
   behaviour — an offset, a `ModeSet`, an epoch — is sent only to a peer that
   declared it, so a drop loses only what an old peer could not use.
3. **Capability views belong to a connection** and are re-probed on every
   re-dial.
4. **Persisted state stays readable by the previous release.** Readers accept
   any version up to the newest they know and ignore unknown fields;
   non-additive artifacts, such as R27's checkpoints, are new files older
   daemons ignore; journal segments never change format; unknown `tear.yaml`
   keys warn, and the refusal is scoped to that entry.
5. **A missing capability selects the old path when the old path is a good
   state** — DECCKM over RPC, the legacy subscribe, lockstep `SendKeys`, the
   full `PaneSnapshot`, offset-0 replay — **and refuses before sending when
   the old path would silently misbehave**, with a typed `Unsupported`, the
   precedent tear's `SpawnEnv` rollout set.
6. **Byte strings need no capability:** ciborium decodes either form in both
   directions for `PaneBytes` and `SendKeys` (P); `Graphic.data` joins the
   probe at R6.
7. **A full-document write never resets what its writer could not see.**
   `SetConfig` carries the writer's tear-config schema version (an additive
   field; a writer that sends none speaks the schema before this plan), and
   the daemon applies only the keys that version knows, keeping every newer
   key's live value — so an older mado's impose, which reads the config into
   its own schema, edits it and writes the whole document back (S mado
   `gui_tear_attach.rs:1811-1825`; daemon `lib.rs:1299-1330`), cannot reset
   this plan's keys, the operator's escape hatches and the gate's controls
   among them. Keys that change durability or delivery semantics —
   `journal.*`, `lanes.*`, `checkpoint`, `sessions.*` — are read once at
   start, as SESSION-DURABILITY §7 already does for `sessions`.

| Pairing | What happens |
|---|---|
| old daemon × new mado | the old capability set selects DECCKM over RPC, the legacy subscribe, lockstep keys and full snapshots; client-side fixes still apply (R3, R10 — `PaneClosed` is already sent — R11, R12, R13); mado answers what it displays, as it does today (R42) |
| new daemon × old mado, the normal state right after a rebuild | no client capabilities, so a `LegacySink`: `PaneBytes`, now byte strings old readers decode, and `PaneClosed`; the replay now carries modes as absolute restores, which old mirrors parse correctly even when they replay twice; a lagging legacy subscriber is never closed — it reads a held pane's journal by offset, or past a process-bound pane's backlog cap receives an in-band resync (R22); the old mado holds the answering lease; its impose keeps every key newer than its schema (rule 7) |
| new daemon × old holder, at every daemon upgrade | `Hello{proto: 1}` with no names, so PROTO-1 verbs only; daemon-side gains apply (fencing through `Attach{from}`, mute detection through `Status.end`, the store lease, checkpoint + tail); holder-side gains (R4, R17, R21, R35's dedup) reach a running shell only through R16, which holders older than R16 lack — they keep their behaviour until their shell exits, which is why R15 and R16 precede the holder's protocol growth |
| rollback: daemon N-1 × holders and documents from N | the holder answers `Hello{proto: 1}` and its names are ignored; documents read (version 1, unknown fields ignored); checkpoints ignored, so offset-0 replay; config rolls back with the binary |
| new mado × a daemon without `view-lane`, after R38 | doorbell-driven snapshot reads per pane through the authority's own snapshot; a pane whose snapshot exceeds the frame cap is refused, typed, for that pane alone; the embedded runtime stays available |

## 8. Tier-honest ledger

### 8.1 Invariants

Tiers are **TARGETS** (status block) in selo's vocabulary, which skill-lint's
tier-ledger check enforces: a CI-caught guarantee is `only-mitigated (C1)`, C1
naming a CI forcing function as its honest terminal; C2 is a fact outside the
process, C4 a shared resource behind a lease. *Red run* names what proves the
gate can see the bad state; *Not covered* names what the tier does not reach.

<!-- tier-ledger -->

| Bad state | Mechanism | Red run | Not covered | Tier (target) |
|---|---|---|---|---|
| a session-hosting daemon rendered in the background band | `session-host` renders only `Interactive`; a `processType` beside a class is not rendered and is named in a warning | a module-trio evaluation test: `session-host` beside `processType = "Background"` renders `Interactive` (pinned output) | a daemon that declares no class; a deliberate `xpc-adaptive`; panes born in the band before R2 that the OS will not reclass | truly-unrep (no class renders it) |
| a case or metric with no budget | the row is the only declaration; `Budgets` has no `Default` | `trybuild`, pinned E0063 and E0004 | enums of mado and madori, red only when mado builds against them | truly-unrep |
| a capability that is never advertised | one `capabilities!` row per variant | `trybuild`: a variant with no row, E0004; mutation A, which left 9 of 9 tests green before the seal (P) | a deliberate macro edit | truly-unrep |
| a key lost after an oversized response | a connection left mid-frame is never reused; a size refusal is never replayed | today's replay policy as a test double: 0 of 20 | other transport faults mid-frame | only-mitigated (C1) |
| a held pane open but mute | a sink owns its connection, and dropping it shuts the connection down both ways | `trybuild`: a sink around a borrowed connection, pinned stderr; the mute-sink fault | holders spawned before R4, which the daemon's `Status.end` check only mitigates | truly-unrep (R4 holders) |
| an incomplete UTF-8 tail reaches a parser | `PaneGrid::feed` takes a `Chunk` only the feeder mints | `trybuild`: a `Chunk` minted outside the feeder, E0451; the old splitter as a test double | — | parse-time-rejected |
| modes read at another instant than the cells | before R38 the mirror (R5); at R38 a sealed `ModeSet`, decoded only inside `OwnedPaneView` | `trybuild`: a `ModeSet` built outside tear-types, E0451; the `daemon-rpc` control | until R38 the mirror trails the authority by the pipeline's latency, as every terminal's parser does | truly-unrep (at R38) |
| a flush on the byte path that nobody chose | `NonZero` intervals; only `write_ahead` flushes before forwarding; rotation and eviction run on the syncer | control `write_ahead{persisted}`; the pump-thread flush counter | a kernel stall of appends behind an in-flight flush (unmeasured; R17's gate) | parse-time-rejected |
| a crash loses more than the declared window | `group_commit{persisted}`; a directory flush after every tombstone and `session.json` rename | a NixOS VM hard reset mid-flood, with a `page_cache` control | macOS has no crash harness; the drive's honesty about its cache | only-mitigated (C2) |
| an unbounded backlog per viewer | bounded rings and a resync; a held pane's byte consumers read the journal; a process-bound pane's keep a capped backlog | `trybuild`: the unbounded queue named without `bench-probes`, pinned stderr; the unbounded-queue fault | a resynced legacy consumer's history has a gap | truly-unrep (no unbounded queue in the production build) |
| bytes delivered twice at attach | attach at offset N under the pane lock; tear producers have no unfenced subscribe | `trybuild`: the two-call subscribe on a tear producer, E0599; engate's two-call path, interleaved | generic engate producers | truly-unrep (tear producers) |
| a gap in a held pane's output passes silently | the holder reports `Gap{from, to}`; `follow` treats an offset past its own as a gap | a 128 MiB flood with the daemon stopped mid-flood | PROTO-1 holders, which clamp without a word | parse-time-rejected |
| a query answered zero or two times | one answering lease per pane, read under the grid lock in fence order; replay below the adoption boundary is silent | the {0, 1, 2 viewers} × {`Prev`, `Head`} × {attach, detach, restart} matrix; the lease-off fault | an old mado in two windows on one pane; renderer-describing answers with no viewer come from a default | only-mitigated (C1) |
| a shared lock held across a PTY or socket write | a pane's writer owns the write half; everything else holds a unit sender with no blocking write | `trybuild`: a write through the sender, E0599; the contention row with a raw-mode child, red on today's code | — | truly-unrep |
| a paste bracket left open or interleaved | the writer owns a paste unit until its end and writes ESC[201~ on every abnormal end | a forced disconnect mid-paste; an agent's keys during a paste | a child that ignores the bracket | only-mitigated (C1) |
| an event conflated away or re-fired by a replay | an append-only ring fenced by offset; overflow is a typed `EventGap`; side effects deduplicated per process | the events row across a clean handoff and a crash, red on today's replay first | loss past retention is surfaced, not prevented; across a crash events are at most once | only-mitigated (C1) |
| a frame published inside a synchronized update | the publisher's gate, with its own deadline | hold set to 0; a BSU followed by silence | the timeout bound | only-mitigated (C1) |
| an unpainted present | `Encoded` is minted only by `PaintTarget::finish` | `trybuild`: `Encoded` built outside, E0451; the BSU-split test | madori consumers on the legacy callback | truly-unrep |
| an acquire while hidden | the acquire helper takes a `Visible` token only non-Hidden states mint | `trybuild`: an acquire without the token, pinned stderr; the state × event matrix | Wayland, where Hidden is inferred from frame callbacks | truly-unrep |
| the UI thread blocks on tear | `LinkHandle` has no blocking method; a structural test keeps clients out of UI modules until R41 makes it E0433 | `trybuild`: a blocking call on `LinkHandle`, E0599; the `ui_io: inline` fault | a deliberate dependency until R41 | truly-unrep (`LinkHandle` holders) |
| an unauthenticated lane served | the unverified lane type has no serve method | `trybuild`, pinned E0599 | — | truly-unrep |
| a lane used on a daemon that never offered it | a `Negotiated<Cap>` witness | `trybuild`, pinned stderr | — | parse-time-rejected (raises SHUKEN §5's capability row) |
| acting on a stale wire view | `apply` refuses a delta whose base is not the replica's epoch, and the replica advances only through it | a replica row with a skipped epoch | a resync costs a keyframe | parse-time-rejected (raises SHUKEN §5's stale-view row) |
| a new pushed frame to a legacy client | `LegacySink` has no method for it | `trybuild` E0599 | a deliberate method | truly-unrep |
| an O(history) transfer | no request asks for more than a page: `PageRows` is bounded at decode; lane frames ≤64 KiB | the `snapshot.history: all` fault; the 100k-row paging row | the legacy `PaneSnapshot` for old clients, refused past the frame cap | parse-time-rejected |
| a paste framed under a stale mode | `PasteModeSkew`, scoped to that paste | a skew row | — | parse-time-rejected |
| a key encoded under stale input modes | input modes published ahead of held cells; `KeyModeSkew`, scoped to that key | a skew row inside a synchronized update | the round trip in C6 | parse-time-rejected |
| a key doubled or lost across a daemon restart | serial dedup at the PTY writer | forced reconnects every 100 keys | at-least-once on holders without `write-serial` | only-mitigated (C1) |
| a `SetConfig` that resets keys its writer could not see | a merge keyed on the writer's schema version; durability and delivery keys read once at start | an impose from `Prev` mado against `Head` | — | parse-time-rejected |
| two authorities on one holder | a store lease ordered by incarnation; `Displaced` is terminal | two daemons on one store for 10 s | PROTO-1 holders obey the lease only through the daemons | only-mitigated (C4) |
| a holder upgrade that ends a shell | a preflight of the new image's adoption ABI; the journal drained before exec; one canary first | a rollback; an image that panics at startup | an image that passes the preflight and fails after the exec | only-mitigated (C2) |
| unbounded history in RAM | `max_bytes` wired | the RSS row with `max_bytes` set | the default is held for the operator (§8.2) | only-mitigated (C1) |
| Nagle on a tear TCP socket | one connect and accept path sets `TCP_NODELAY` | an rg gate; the Nagle probe | a bypass outside the profile | only-mitigated (C1) |
| a shell started outside the default band | the holder resets its main thread and clears inherited background state on the child before exec | the `proc_pidinfo` row through the production `NewSession` RPC, the daemon at `session-host` and at `xpc-adaptive` | whether `posix_spawn` carries the caller's QoS (§8.3) | only-mitigated (C1) |
| an allocation per line feed | row recycling | the counting allocator; the allocating-row fault | — | only-mitigated (C1) |

### 8.2 Held for the operator

| Decision | Evidence | Until decided |
|---|---|---|
| The journal's flush primitive | `persisted` keeps what SESSION-DURABILITY §3.3 promises — a crash loses at most the window — at 3.73–5.92 ms a flush (F-c, D). `ordered` costs 0.22–1.41 ms (F-c, D), 3–17× cheaper within either probe, and keeps ordering, but promises nothing persisted at return (`man 2 fcntl`); the two probes disagree by 6.4× on the barrier, so R1 re-measures both primitives in one run before the decision. The survival ladder (§2 there) names process deaths, logout and reboot, where the page cache loses nothing either way; only a panic or a power loss tells them apart. | `persisted`, off the byte path (R17) |
| The scrollback default | unlimited rows is a recorded contract (S `tear-config` `lib.rs:238`). Held panes journal at most 64 MiB (S `tear-config` `lib.rs:219-220`) and re-adoption rebuilds the grid from that journal alone (D), so history beyond it already disappears at every daemon restart; a 64 MiB bound in RAM would make the two horizons agree. Compact history (R26) makes either affordable. | unlimited, stored compactly |
| The holder's growth — SESSION-DURABILITY §3.2 keeps it "deliberately small and slow-moving" | a holder fault ends a live shell, the one loss the design exists to prevent. R16 makes the holder upgradable in place behind a preflight and a canary, which no holder is today; R21's holder half moves the pump from 1 KiB frames to 64 KiB batches (the journal-and-framing hop 407 → 1,130 MiB/s, D) and serves observers and a lease-ordered authority. Each adds code to the one process whose crash is a lost shell. | R16 ships with `durability.holder_upgrade: never`; R21's local half lands, its holder half waits |
| The checkpoint — SESSION-DURABILITY §3.3 chose "raw bytes, not rendered cells" | a restart re-parses a pane's whole journal, 0.59–0.71 s per 64 MiB with the child paused (G); a checkpoint plus a ≤4 MiB tail costs 46–47 ms of parsing (G) plus its load. It is a second serialisation of the grid, which must carry what `to_ansi` drops today (S) or fall back to offset 0, and espelho equality is its gate (R27). | `checkpoint: off`: offset-0 replay |
| Who owns praça | the daemon keeps a praça store and mado keeps its own file, and no wire request reads the daemon's (S); R18 makes the daemon bind resident sessions | both, as today |
| Who owns a shared pane's size (C5) | latest-focused, largest, smallest or manual; tmux defaults to latest (R) | latest-focused |
| The present mode on macOS | the deployment runs Immediate on macOS (`vsync: false`, L) and Fifo on Linux, after measuring tearing there; Ghostty keeps vsync on, citing kernel panics with out-of-sync rendering on macOS 14.4+ (R) | unchanged until R1's input → present histograms compare both under R11 |

### 8.3 Not yet measured

- C5 with several windows, C11 resize timings, launch to first frame, battery,
  and idle wakeups — top's `IDLEW` read 0 for every process, a blind probe, so
  context switches stood in (L).
- The foreground window: L sampled an App-Napped mado, so its tick rate and
  band in front were not read.
- GPU time per frame (no timestamp queries yet), and the Ctrl-S frame since
  `aa031a2` cached overlay shapes (R1 for both).
- Whether launchd's background state on a running process can be cleared from
  outside it (R1, for R2), and whether `posix_spawn` carries the caller's
  thread QoS into the child (R28).
- Linux: the PTY read quantum, the cost of `fdatasync`, and whether per-pane
  scopes measurably help (R2's Linux gate).
- Whether an acquire on a hidden Wayland surface under `AutoVsync` blocks the
  UI thread (R11), and whether macOS's compositor can be handed damage through
  `CAMetalLayer` (R31).
- Remote links: ssh, WebSocket and real networks; whether OpenSSH sets
  `TCP_NODELAY` on forward-only channels (R39's Nagle row decides).
- mado's own VT throughput (its benches cover only motion curves); it sizes
  the pre-flip view lane's batch budget and tells how often a legacy
  subscriber lags (R22).
- Whether an in-flight `F_FULLFSYNC` stalls appends to the same file (R17's
  gate).
- Whether unpainted presents fully explain the afterimage reports (R12's A/B).
- GitHub's macOS VM: whether it exposes Metal and real flush semantics; cells
  it cannot measure become typed `NotApplicable` in CI and stay measured on
  the reference Mac.
- The event ring's defaults (1,024 events or 1 MiB) and the OSC 52 payload
  cap; clipboard size is a security surface.
- Per-cell truecolor content makes an 87,351 B keyframe at 514 µs of encode,
  dominated by style interning (C): whether runs fall back to inline styles
  past ~4,096 new styles a frame.
- Whether a backgrounded browser tab, which stops animation frames, needs an
  explicit paused watch.
- Whether each new C1 pane forks the GUI process (a hypothesis from code, G).
- Holder `reexec` under systemd `KillMode=process`.

## 9. What this is not

- **Not a shared-memory transport.** A futex-woken ring is no faster than UDS
  for one message (F-e), and in-process frames are already RCU. Revisit only
  if the gate shows view-lane encode + write + read + decode above 5 % of a
  120 Hz frame (417 µs) per consumer.
- **Not a data-plane bypass.** mado does not read the holder over a passed
  descriptor: that would bring back a second parser. The multi-sink holder
  serves upgrade handoff and lossless observers; embedded sessions stay
  non-durable, as SESSION-DURABILITY §5 types them.
- **Not XPC.** The band is fixed by declaration (R2), not by importance
  donation; tear keeps UDS.
- **Not a bigger frame cap.** 16 MiB stays a decoder sanity bound; lane frames
  never exceed 64 KiB.
- **Not a reversal of the durability or scrollback decisions.** This plan
  moves where flushes run and how history is stored; its two amendments to
  SESSION-DURABILITY and every default that would change a guarantee are held
  for the operator (§8.2).
- **Not a second parser in mado, even for a while.** R30's interim frame is a
  projection of the parser mado already has.
- **Not a present-mode change on macOS, not `CAMetalDisplayLink`, and not a
  higher minimum macOS.**
- **Not a compositor win.** wgpu's `present()` carries no damage; R31 saves
  mado's own CPU and GPU.
- **Not Linux zero-copy** (`splice`, `tee`): the parser, not the copy, bounds
  throughput (H-d).
- **Not a restatement of SHUKEN or SESSION-DURABILITY.** It schedules their
  remaining work, names its three amendments, and measures it.
- **Not done when the rungs land.** Done is every cell `Within` on the
  reference Mac, each with its red run recorded.

## 10. Stale models to correct

All land with R0 unless named; each corrects the body, not a footnote.

| Where | Says | Is (receipt) | Fixed at |
|---|---|---|---|
| SESSION-DURABILITY §3.2 | the holder is "double-forked + `setsid`, so it is nobody's child" | one `Command::spawn`; the holder calls `setsid` itself and stays the daemon's child, reaped by a thread (S tamotsu `held.rs`, `holder.rs:54`) | R0 |
| SESSION-DURABILITY §3.2 | the holder protocol is versioned by a Hello-style capability probe | an integer, `PROTO = 1`, refused on any other value (S `holder.rs:206`) | R15 |
| SESSION-DURABILITY §3.3 | "fsync cadence: every 1 s or 1 MiB", silent on where | inline on the PTY pump, under the holder lock, before forwarding, and at every segment rotation (S `journal.rs:128-158, 214-217`, `holder.rs:163-172`) | R17 |
| SESSION-DURABILITY §3.3 | cwd is polled with `proc_pidinfo` on macOS | Linux only, as §7 of the same document says | R0 |
| SHUKEN §3 | `PaneView<'_>` borrowed in process | C1 reads an `Arc<OwnedPaneView>` published per parse batch; a borrowed view would hold the parser's lock while a renderer reads — the amendment named in the status block | R26 |
| SHUKEN §5, DSR/DA row | "NOT CORNERED" | partly cornered since `de5afbd`: `PaneGrid` answers DSR 5/6, DA1 and DA2 under the host role, but nothing outside tests sets the role or drains the answers (S `pane_grid.rs:1615, 1625`) | R42, R43 |
| SHUKEN §5, modes-instant row | "`ModeSet` is a field of `PaneView`, never a separate request" | `ModeSet` is pub-field, `Default` and `Deserialize`, and a separate fetch exists (S `modes.rs:124-135`, `control.rs:311-327`) | R38 |
| SHUKEN §5-C | "what is left is the migration itself" | six more preconditions: kitty keyboard flags and their siblings untracked (S `modes.rs:125-135`); resize truncates (S `pane_grid.rs:1729-1731`); the host role is never set outside tests (S `pane_grid.rs:1615`); nothing drains `take_response` (S `:1625`); answer parity — DECRQSS misrouted to sixel, 18t, the OSC colour and clipboard queries, `CSI ? u`, DECRQM; state parity — 14 OSC codes beyond SHUKEN's own OSC 8, and BEL (S `pane_grid.rs:1039-1042, 1075, 1368-1404`). The repoint counts re-measure at 25 `term.*` calls behind 4 lock sites in `render.rs` and 87 behind 36 in `ux/engine.rs` (70 through a binding, 17 chained) | R5, R26, R42, R43 |
| mado GRID-THREADING-CONTRACT | parsing is off the main thread; the mailbox is the next step | in the tear runtimes chunks are parsed on the AppKit thread, up to 4,096 messages a tick (S `gui_tear_attach.rs:1137-1141`); this plan implements `PauseReader` at the source (R21) and the document points here | R0 |
| mado GRID-THREADING-CONTRACT | "the terminal NEVER drops bytes" | that binds the authority; a view resyncs from it (R22, R34) | R0 |
| mado REMEDIATION-PLAN §M7 and GRID-THREADING-CONTRACT's "never restructured again" | M7 owns the render decouple, damage and the mailbox | on the tear path these are R13, R30 and R31; M7 keeps the local-PTY path until R38 | R0 |
| mado's contributor guide, threading section | a PTY thread reads and parses | true of the local-PTY runtime only | R0 |
| mado `gui_tear_attach.rs:681-684` | a dropped VT-query answer kills reedline-based shells | true of upstream reedline; frost builds against pleme-io's fork, whose painter falls back after a failed cursor report (S reedline `painter.rs:204-224`), so there a dropped answer costs 2 s per cursor report (R crossterm) | R0 |
| tear-types `wire.rs:10-11` and `:28-32` | CBOR's cost is negligible; appended variants are safe | byte payloads cost 1.98× the bytes and 60–173× the round trip (F-f); appended *responses* pushed unprompted break old clients | R0, R6, R15 |
| tear-types `engate_wrap.rs` | attach cost is cells × 4 | it omits scrollback, graphics and encoding: 469,816 B + 9,782 B a row (H) | R0 |
| tear-types `capability.rs:309-311` | a variant missing from `ALL` fails this assertion | it passes (P) | R0 |
| tear-types `host_role.rs:68-83` and its test | tear has no sixel: `GridState` implements no `hook`/`put`/`unhook` | `PaneGrid` parses sixel since `e7c6ba7` (S `pane_grid.rs:1039-1061`); what DA1 advertises should describe the viewer's renderer | R43 |
| tear-client `lib.rs:27-30, 58-60` | separate `Client`s avoid head-of-line blocking; the client never pumps kilobytes a second | mado shares one `Client`, and `PaneBytes` flows through it (S) | R0 |
| tear-core `pane_grid.rs:2305` | a snapshot is not a defect: it is off the hot path | it runs on every key and every attach (H) | R9 |
| tear-daemon `serve_subscription` | the overlap is harmless because ANSI sequences are idempotent | false for line feeds, scrolls, IL/DL and relative moves (S `lib.rs:990-996`), and history is replayed twice (Gate 0 class 8) | R5, R20 |
| tear `.github` headers | CI is dormant | `ci.yml` is disabled at the GitHub level; auto-release's default-feature test gate is the only one running (S, Actions API) | R0 |
| mado `render.rs:2118-2122` | afterimages come from slots that keep what they held under `LoadOp::Load` | every painted frame clears in pass 1 (S `render.rs:7117-7134`); only an unpainted present can show old content | R12 |
| mado `render.rs:6419-6423` | `draw_overlay` bypasses the shape cache; ~4,900 µs a frame | `overlay_shapes` caches it since `aa031a2` (S `render.rs:1776, 3141`) | R1 |
| mado's fps comments (`gui_tear_attach.rs:643-648`, `main.rs:1089-1095`) and the deployment's own | the tear path budgets against the real frame rate "instead of the hardcoded 60 Hz floor"; "the default is unchanged"; `target_fps: null` means no ceiling | `resolve_target_fps(None)` returns `FALLBACK_FPS = 60` (S `config.rs:2702-2723`), so the tear path runs `Capped(60)` (L) | R0 |
| mado `mcp.rs:410` | `frame_perf` returns zeros when no GUI is reachable | it must answer blind | R1 |
| tear-core engate producer docs and mado `gui_tear_attach.rs:180-181, 1399-1405` | embedded mode parses once; "no second VT parser" | every byte is parsed twice in C1 (S `inproc.rs:631`) | R0 |

## 11. Provenance

- **The 2026-10-07 measurement pass.** Seven code reads covered the hot paths
  — mado's consumer loop, mado's rendering and ingestion, the tear-client
  wire, the tear daemon, the durability chain, tear-core's embedded grid, and
  multi-attach, remote and observers — and recorded 131 defects with locations
  and confidence, 117 confirmed in code. Three measurement campaigns ran
  beside them: a passive profile of the live system (L), an isolated harness
  (H) and a floor suite (F); four surveys covered prior art (terminal
  architectures, multiplexers and persistence, IPC and systems theory, macOS
  frame pacing). The design pass added the durability probes (D), the grid
  benches (G), the view-codec prototype (C), the compatibility probes and a
  mutation of the capability pin (P). Their sources and raw results arrive in
  this repository with R1 (Receipts).
- **Harness cases.** (a) back-to-back RPCs, n=3,000; (b) closed-loop echo
  after a 2 ms gap, n=1,500, then 1,000 mado-shaped keys and 1,000 without a
  gap; (c) an open-loop 15 s series of one key per 10 ms, n=1,500, run twice;
  (d) 64 MiB floods in 3 reps, plus a raw PTY ceiling; (e) echo while a second
  client polls at 300 Hz; (f) attach after 0.25–8 MiB of history, 3 reps; plus
  the snapshot cliff (0–3,000 lines, n=10 per depth), key loss (3,000 lines,
  20 + 5 keys), cold starts (5 × 5), restart with an 8 MiB journal (3 reps),
  connects (30) and wire microbenchmarks.
- **Machine.** The reference Mac: Mac16,7, Apple M4 Pro with 10 performance
  and 4 efficiency cores, 48 GiB, macOS 26.7.1 (25G241), APFS with FileVault
  on the internal SSD, on AC power, its built-in Liquid Retina XDR the only
  display (ProMotion, up to 120 Hz on this model; the rate was not read, L).
  rustc 1.98.1; ciborium 0.2.2, serde 1.0.229 and serde_bytes 0.11.19, the
  versions tear locks.
- **Revisions.** tear `d017810` (0.1.32, the live binary); mado `ec50bfb`
  (`render.rs` and `gui_tear_attach.rs` identical to the live 0.1.180); madori
  `73b2511` (0.1.21); garasu `c4efeed`; engate `e079c79`; substrate `67709f8`;
  kanshou `4743a3e` (0.1.9); theory `ca3d4c4`; frost `894bb4e`, which pins
  pleme-io's reedline fork at `5643ba0`; skill-lint `b66c85b`; selo `422412f`;
  pleme-io/actions `6f578a9`.
- **Isolation.** The harness re-executed itself with a cleared environment and
  every home, state and temporary directory under a scratch root, refused to
  run otherwise, and checked 13 daemon and holder snapshots for files open
  outside it (0 found). The live system was only observed: no keys, attaches
  or signals.
- **Load.** The machine was shared and loaded: load average 12.5–15.5 during
  the harness, 4–30 during the floor suite, 24.5 and 40.2 for the two codec
  runs, 16.7–35 during the live profile. Medians, the mado-shaped key and the
  1 Hz spike reproduced on rerun; extreme maxima did not (235 ms became 10.8
  ms), which is why §6 never gates a maximum.
- **Emulation and limits.** The background band was emulated with `taskpolicy
  -b`, matching the live processes' observable state (thread priorities 4/4/4,
  a child's 4/4/63); launchd's own policy was not introspected. The variant
  `held-bgd` puts the daemon and holder there with the client at PRI 31, as
  `Adaptive` does; `held-bg` puts the client there too, as App Nap does. Echo
  was measured to the consumer's receive thread, not to a rendered frame. D's
  pause probe measured the holder hop alone. What was not measured is listed
  in §8.3.
- **Corrections made while integrating.** The frame codec needs no new crate —
  tamotsu already reaches tear-types through makimono (S `Cargo.toml`) — but
  its socket calls must stay unix-only, because mado-web is a wasm32 crate
  that depends on tear-types (S). tear's CI is not simply off: auto-release's
  test gate runs, on default features only, so the `engate` feature's tests
  compiled nowhere (S, P). The snapshot replay does carry scrollback —
  `to_ansi` lays it down before the grid (S `pane_snapshot.rs:381-399`) —
  despite one code read. The live present mode on macOS is Immediate, not Fifo
  (`vsync: false`, L). "~29× the bytes printed" belongs to a 128 MiB `cat`
  (M); a 64 MiB flood is ~34× (H-d). A cheap modes query, proposed during the
  pass, would reopen the skew the mirror avoids, so the interim reads mado's
  mirror. Hand-set `ProcessType` is not a dozen sites but 32 literal settings
  in 27 files (counted 2026-10-07, comments and tests excluded).
- **Corrections made in review.** Echo floors counted the receiver's wake
  twice; a hop is a send plus one wake. The cost of `Adaptive` is the
  `held-bgd` variant's, not `held-bg`'s, whose client was App-Nap-shaped. The
  886 µs frame is one last-frame reading, beside an earlier 3,634 µs. A
  restart accepted in 3–6 ms at earlier logged starts; 540–547 ms was the file
  watcher at the last two. Ghostty's +15 % is on the change, not in total. The
  ~1,022 B input bound holds for raw-mode PTYs only. 120 Hz is the panel's
  maximum, not a reading. madori's pacing documentation is accurate; the stale
  pacing comments are mado's. `ux/engine.rs` makes 87 `Terminal` calls, not 70.
  The flip lacks six preconditions, not three. An old mado's
  non-switchable window reads a closed stream as its shell's exit, so no
  legacy subscriber may be closed. A band is inherited at spawn, so panes
  alive across R2 need reclassing. An unanswered cursor report costs frost a 2
  s stall, not its shell: frost builds against pleme-io's reedline fork, which
  falls back (S).
- **From R1 on**, the harness and the floor suite live in `tear-bench`: every
  number here is re-measured there, not trusted.

