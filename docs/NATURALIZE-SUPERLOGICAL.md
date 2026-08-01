# naturalize(Superlogical) — granting the block citizenship

> **Read this caveat before any row below.** Superlogical has **shipped
> nothing** — no beta, no version, no artifact, GitHub org with zero public
> repos, waitlist only (verified 2026-07-31, recorded in the
> `reference_superlogical_hashimoto_multiplexer` memory). So the "what X will
> be" column is **INFERRED, not observed**, and every claim of parity against
> it is a claim against a *prediction*. That is a weaker epistemic position
> than any other naturalize in the fleet, and pretending otherwise would be the
> exact round-up this doc's ledger exists to forbid.
>
> **What the inference is built from** — four primary sources, no speculation
> beyond them:
> 1. **Their own job board** (`ssh superlogical.jobs`): Go core, Nix/NixOS/Go/Linux
>    infra, TS/JS web, ObjC/Swift/SwiftUI Apple, native Windows. No Linux client
>    promised. Zero Zig roles — a fact about *hiring*, not about the codebase.
> 2. **Their stated technical claim**: legacy multiplexers "sit between your
>    terminal emulator and the pty, causing double parsing and duplicated state
>    processing."
> 3. **Their product model**: "terminal blocks" (Warp-style), *not* tmux panes.
> 4. **Category table stakes** for a terminal multiplexer: persistence,
>    detach/reattach, remote sessions.
>
> Anything not traceable to those four is marked SPECULATIVE in the ledger and
> carries no parity claim at all.

---

## The destination, unhedged (Op#0)

**The pleme-io-native Superlogical is the block as an *attributable, brakeable,
sealed unit of work* — a terminal history that can answer "who ran this" by
construction rather than by correlating logs.**

Superlogical's product unit is the terminal block. So is ours, and has been.
The difference we can hold — and the reason this is a naturalize rather than a
feature-race — is that pleme-io already ships an **identity spine** (`shutai` →
`yurai` → `freio`) that a Go multiplexer with no identity model cannot retrofit
cheaply. Their block records *what happened*. Ours can record *who made it
happen*, and can stop them.

That is the widened algebra, stated plainly: **before, a block history could not
distinguish an agent-run `rm -rf` from an operator-run one. Now it can, and the
distinction is carried by the artifact instead of reconstructed from logs.**

---

## Recon — most of Superlogical already exists here (Op#1)

The naturalize recon result is the usual one: the substrate already covers most
of it, and the compounding win is composition, not a fresh build.

| Superlogical (inferred) | source | pleme-io realization today | state |
|---|---|---|---|
| Terminal blocks as the product unit | product model | `tear_types::Block` + `tear_core::BlockExtractor` (OSC 133 A/B/C/D state machine, OSC 7 cwd, 10k ring) | **SHIPPED** |
| No double-parse between emulator and pty | their technical claim | **shuken** — `PaneGrid` is the SOLE VT authority; `PaneView` has no write verb (`E0599`), `vte` leaves mado's manifest (`E0433`) | **SHIPPED** — `docs/SHUKEN.md` |
| Persistent detach/reattach sessions | category | tear daemon + `praca` session platform, frecency-ranked Ctrl-S union navigator | **SHIPPED** |
| Native macOS client | job board (Swift/SwiftUI) | **mado** — GPU, all-Rust, native `UNUserNotificationCenter` | **SHIPPED** (and all-Rust rather than SwiftUI) |
| Web client | job board (TS/JS) | `mado-web` + `tear-ws-bridge` | **SHIPPED-partial** — exists; not measured against a TS client that does not exist |
| Go core daemon | job board | `tear-daemon` in Rust | **SHIPPED** |
| Session recording / replay | category | `tear_types::cast` — asciinema export, epoch-anchored | **SHIPPED** |
| tmux interop | category | `tear-tmux-backend` | **SHIPPED-partial** |
| **Attribution of a block to an actor** | — *not theirs; ours* | `Block.yurai`, stamped write-once at `PaneGrid` | **SHIPPED (this doc)** |
| **A brake that stops automation mid-session** | — *not theirs; ours* | `freio` — `Admission::Refuse(RefusalReason::Freio)`, `unbrakable()` names what it could not reach | **SHIPPED** |
| Native Windows client | job board | — | **ABSENT** |
| Block sharing / permalink (the SaaS hook) | SPECULATIVE | — | **ABSENT** |
| Real-time multi-user collaboration | SPECULATIVE | — | **ABSENT** |
| Cross-session block search corpus | SPECULATIVE | — | **ABSENT** |

**Three ABSENT rows are SPECULATIVE** — inferred from "it is a funded company
and needs a SaaS hook", not from anything Superlogical has said. They are listed
so the gap is visible, **not** as a roadmap. Building against a guess about a
competitor is precisely the market-chasing the operator has already ruled out:
*"I don't care about the market, I just wanna build beautiful things."*

---

## What landed here (the net-new delta)

`Block` was **anonymous**. Every field described *what* happened — prompt,
command, output, exit code, timing, cwd — and none described *who*. In a
terminal that is explicitly driven by agents through MCP, that is a real hole:
the block history could not tell an agent's command from the operator's.

Now `Block.yurai: Yurai` is stamped at prompt start, from the owning pane's
provenance, at `PaneGrid` — the same seam shuken made the sole VT authority,
because a second path that attributed blocks anywhere else would reintroduce
exactly the duplicated-state split shuken exists to forbid.

Three properties, each with a reason:

- **Non-`Option`.** Every block answers the question; `Unknown` IS an answer and
  the honest one. An `Option` would let a consumer skip the question — the very
  state the field exists to remove. This is enforced by the compiler: adding the
  field broke a construction site in `tear/src/ai.rs` with `E0063`, which is the
  guarantee working, not a papercut.
- **Defaults to `Unknown`, never `Human`.** Same discipline `Yurai` itself
  follows, for the same reason: assuming human would make `freio` silently skip
  a pane it was pressed to stop, and would launder every agent-run command in a
  pre-field history.
- **Write-once.** A settable field would contradict the type it carries —
  `Yurai` is a pane's provenance *for its whole life*. A re-stampable field
  would let an agent-spawned pane relabel itself `Human` mid-session and
  retroactively launder every block after that point.

---

## The required deliverable — tier-honest ledger

Tier vocabulary is closed (`selo::SealTier`): `truly-unrep` ·
`parse-time-rejected` · `only-mitigated (C1..C6)`. Every `only-mitigated` row
**names its ceiling**.

<!-- tier-ledger -->

| capability / bad state | pleme-io realization | tier |
|---|---|---|
| a `Block` that cannot answer "who ran this" | the field is non-`Option`, so every construction site must state it — omitting it is `E0063` (observed, not asserted: it broke `tear/src/ai.rs` on introduction) | truly-unrep |
| a pane spawned with its blocks UNATTRIBUTED | `yurai` is a parameter of `spawn_pty_for` — the one choke point every pane's grid is created at — so omitting it is a compile error rather than a forgotten follow-up call | truly-unrep |
| the attribution wire existing but not DELIVERING | `an_agent_spawned_session_produces_attributed_blocks` asserts through `pane_blocks_list`, the surface an operator/MCP client actually reads. **Red-run verified**: replacing the stamp with `let _ = yurai;` reproduces the exact defect shipped in `fe50cf4` (`left: Unknown, right: Automation{"claude-code"}`) | only-mitigated (C1 — a test, not a type; it catches the unwired state rather than making it unconstructible) |
| mado grows a second VT parser | shuken: `vte` is absent from mado's manifest → `E0433` | truly-unrep |
| a `PaneView` consumer writes to the grid | shuken: no write verb exists on the type → `E0599` | truly-unrep |
| a pre-attribution block decoding as "human" | `#[serde(default)]` + `Yurai::default() == Unknown`; pinned by `a_pre_attribution_block_decodes_as_unknown` | parse-time-rejected |
| provenance DRIFTING mid-pane (agent relabels itself human) | `stamp_yurai` is write-once and refuses the second call; red-run verified — removing the guard fails `re_stamping_is_refused_so_provenance_cannot_drift` on its stated assertion | only-mitigated (C1 — a runtime guard, not a type; a `Yurai`-typed const generic would be needed to make drift unconstructible) |
| a FABRICATED `Automation` provenance with no attested connection | `Yurai::from_shutai` is the only production path | only-mitigated (C1 — `Yurai::Automation` is a public variant, so in-process code CAN construct one; enforced by convention + `yurai.rs` tests, not by the compiler) |
| an automation pane escaping the brake | `freio` consulted before the policy lattice; `unbrakable()` enumerates panes it could NOT reach | only-mitigated (C2 — a pane whose provenance is `Unknown` is not braked, and unknown-ness is an external-world fact the daemon cannot settle) |
| Superlogical capability parity | — | **NOT CLAIMED.** X has shipped nothing; there is no differential to run, so no parity row can go green |

---

## Citizenship check — the four falsifiers

1. **Native?** Yes. Nothing of Superlogical is vendored — there is nothing *to*
   vendor. The block model was independently shipped here before their
   announcement.
2. **No leak?** Yes, vacuously — no Superlogical wire type, config format or API
   vocabulary exists to leak, because no artifact exists.
3. **Proven, not asserted?** **Partially, and this is the honest limit.** The
   *internal* guarantees are proven (tests green, one red-run per new guard, an
   `E0063` observed). **Parity with Superlogical is unprovable in principle
   today** — you cannot run a differential against a waitlist. Any future claim
   of "we match Superlogical" must wait for an artifact to differ against.
4. **Known to the tribe?** This doc, cross-linked from `SHUKEN.md`; the memory
   entry is the default-read-path anchor.

**Verdict: a resident, not a full citizen** — and by this skill's own rule that
is honest rather than a failure, because the work is rebuilt-not-vendored and
the tier is labelled. It cannot become a full citizen until X ships something to
be a citizen *relative to*.

---

## What this does NOT claim

- **Not** "we beat Superlogical." They have shipped nothing; there is no race
  with a competitor who has no artifact, and the operator has explicitly
  declined to run one.
- **Not** "blocks are new here." `BlockExtractor` predates their announcement.
  The delta is attribution.
- **Not** "attribution is proof." `Yurai::Automation` records a *claim* made by
  a connection, at the tier the claim was made. A peer that lies about being
  human is recorded as human. `Shutai` keeps `Attested` (kernel-verified via
  `getpeereid`/`SO_PEERCRED`) and `Declared` (peer claim) as separate tiers
  precisely so this boundary stays visible — and `yurai` is a projection of the
  *declared* half.

---

## Two failures found while building this — both recorded, neither hidden

**1. The attribution shipped DECLARED BUT UNWIRED** (`fe50cf4`). `stamp_yurai`
landed with green unit tests and *nothing in production called it*, so every
real block would have read `Unknown` forever while the suite stayed green. This
is the same declared-but-uninvoked trap the nix repo's `CLAUDE.md` documents for
`checks` entries, reproduced in Rust. The fix was not "remember to call it" —
it was to make `yurai` a **parameter of `spawn_pty_for`**, so omitting it is a
compile error. The lesson generalizes: *a guarantee reached only by a call
someone must remember is not a guarantee.*

**2. A test in the suite was FLAKY, and it cost a false regression signal.**
`list_source_filter_shows_only_matching_sessions` asserted
`!stdout.contains("a1")` against output whose every row begins with a random
16-hex-digit session id. Observed: a single *correct* row,
`4a059f81739ba133 h1 …`, failing on the `a1` inside `9b`**`a1`**`33`. Fixed by
matching the name column as a whitespace-delimited field, pinned by a test that
asserts both that the raw substring still collides *and* that the helper rejects
it. A test that fails on a coin flip is worse than no test — it trains the
reader to dismiss a red run, which is exactly when a real regression slips past.

**Cross-reference:** [`SHUKEN.md`](./SHUKEN.md) (the authority seam this builds
on), [`SESSION-TYPESCAPE.md`](./SESSION-TYPESCAPE.md) (the typed model),
[`SESSION-FEATURESET.md`](./SESSION-FEATURESET.md) (operator projections).
