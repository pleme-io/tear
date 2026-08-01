# shuken (主権) — the authority seam

> **Status: DECISION RECORD, destination-first. Nothing here is shipped.**
> This doc records an operator decision made 2026-07-31 and the typed shape
> it implies. Implementation recon was in flight when it was written; every
> tier below is a **TARGET**, not an achievement. Per the pleme-io
> UNREPRESENTABILITY discipline a `Result::Err` is *mitigation* and a
> compile error is *unrepresentability* — the ledger in §5 grades targets,
> and nothing may be re-graded without a red run against a deliberately
> broken input.

> **Name: proposed, not ratified.** 主権 *shuken* = sovereignty. Japanese
> per naming law 1 (foundational substrate); the literal gloss should let a
> reader guess the job — *who holds the right to mutate*. Veto freely.

> **Gate 0 lives elsewhere — read it first.**
> [`theory/MADO-TEAR-SEAM.md`](https://github.com/pleme-io/theory/blob/main/MADO-TEAR-SEAM.md)
> is the seam's Gate 0 (written 2026-07-30, measured 2026-07-31: ~90 illegal
> states in eight classes). **This doc records a DIRECTION; that doc records
> the STATES.** Do not restate its list here. shuken answers its §IV — "what
> would speaking the same language mean" — with *authority* rather than
> content, and inherits its §VII preconditions.

---

## 1. The decision

**`PaneGrid` is the sole authoritative VT parser for a pane. Everything
mado's `terminal.rs` parses today is promoted into it. mado owns no
terminal state.**

This resolves the either/or that [`SESSION-TYPESCAPE.md`](./SESSION-TYPESCAPE.md)
§7 left open at M5. That doc offered two ways to close the no-overlap gap:

- *grow the snapshot to own graphics*, or
- *restate no-overlap as "text non-overlapping; graphics intentionally
  mado-local."*

The second is a partition — it keeps two parsers and renames the overlap a
boundary. **We take the first.** One parser, no exceptions, including
kitty graphics, sixel, OSC 8 hyperlinks, synchronized output (mode 2026),
bracketed paste, styled underlines, and the interned style/link tables.

## 2. Why the seam is *authority*, not *content*

The tempting seam is a content split — text here, graphics there. That
seam has to be re-litigated every time a new escape sequence lands, and it
gives a client a legitimate reason to keep a parser around. It does not
close the class.

The seam that closes the class is **ownership**: exactly one type may
advance VT state, and what crosses to a client cannot advance anything.

[`SESSION-TYPESCAPE.md`](./SESSION-TYPESCAPE.md) §1 already asserts this —
*"a view is a read projection: it chooses which pane it displays and at
what geometry, but it owns no session state."* It has always been true as
**prose** and enforced by **nothing**. The live double-parse is the proof
that prose does not hold a boundary. shuken makes the same sentence a
type.

## 3. The typed shape

```
tear-core            PaneGrid          holds the vte::Parser + every mutation verb
                       .advance(&mut self, &[u8])      ← the ONLY write path, tear-side
                       .view(&self) -> PaneView<'_>    ← the ONLY thing that crosses

tear-types           PaneView<'a>      borrowed cells, cursor, style table, link table,
                                       graphics placements. NO advance. NO parser handle.
                                       NO &mut anything.

mado                 (a renderer)      consumes PaneView. Cannot parse: see §4.
```

A client holding a `PaneView` has no method that feeds it bytes, so
"the renderer advanced the grid" is an **absent method — `E0599`**, not a
convention and not a review rule. Same construction as banken's
`ClusterEnv` having no unwitnessed-mutate method.

## 4. The load-bearing seal: mado does not declare `vte`

`PaneView` having no write verb stops a client from mutating *the view*.
It does not, by itself, stop mado from constructing its own parser
alongside — which is exactly how the present double-parse arose.

Dependency absence closes half of it: **`vte` is removed from mado's
manifest.** A transitive dependency is not nameable without being declared,
so `vte::Parser` in mado becomes **`E0433` unresolved crate**.

> **★ CORRECTED, THEN SEALED — both on 2026-07-31.**
>
> **The gap:** dependency absence was never sufficient. mado declares
> **`tear-core`** directly (`mado/Cargo.toml:179` — 57 call sites, required
> for `InProcess`), so with `vte` gone this still compiled:
>
> ```rust
> let mut g = tear_core::PaneGrid::new(80, 24);
> g.feed(bytes);            // a second authoritative grid, no `vte` named
> ```
>
> **The seal, and why it is NOT a Cargo feature.** A `parser` feature was the
> first proposal and it is the wrong mechanism: mado *needs* `InProcess`,
> which owns the PTYs and drives these grids internally — so a feature that
> excluded `pane_grid` from mado would break the runtime this whole decision
> depends on, and one that included it would seal nothing.
>
> What actually seals it is **visibility**: `PaneGrid::{new, with_scrollback,
> feed}` are `pub(crate)`. Outside `tear-core` the constructor and the write
> verb are private items. Measured from `tear-daemon` (an external crate that
> links `tear-core` exactly as mado does):
>
> ```
> error[E0624]: associated function `new` is private
> error[E0624]: method `feed` is private
> ```
>
> Reading stays public — `snapshot()`, `PaneSnapshot`, `Cell`. **The
> asymmetry IS the design: anyone may read, only the authority may advance.**
>
> One consequence worth knowing: `tests/espelho_conformance.rs` moved to
> `src/` and became a unit test. An integration test links the crate as an
> external consumer, so the seal denies it exactly what it denies mado —
> correctly. Being *inside* the crate is what lets it keep testing the sealed
> surface.

Grade this honestly (§5): re-adding a dependency is a deliberate,
reviewable act, and a forcing-function test asserting `vte ∉ mado`'s
manifest is *only-mitigated* against someone choosing to. The
**accidental** regrowth of a second parser is truly-unrepresentable; the
**deliberate** one is mitigated by a gate that has to actually run — and
mado has no test CI today, which is a named precondition of this work, not
a footnote.

## 5. Target tier ledger

<!-- tier-ledger -->

> **★ AMENDED 2026-07-31, same day, against measurement. Two rows below were
> over-graded when this doc was written — including the one §4 calls
> load-bearing.** They are corrected in place rather than quietly restated,
> because a ledger that silently re-grades itself is worth nothing.

| bad state | how the vocabulary corners it | tier (TARGET) |
|---|---|---|
| a renderer advances VT state through the view | `PaneView` exposes no `&mut self` and no byte-feeding verb — `E0599` | truly-unrep |
| a client fabricates a view no grid produced | private fields, no public ctor; only `PaneGrid::view()` mints one — `E0451` | truly-unrep |
| mado names `vte::Parser` | `vte` absent from mado's manifest — `E0433` unresolved crate | truly-unrep |
| **mado builds a second authoritative grid** | **SEALED 2026-07-31.** `PaneGrid::{new, with_scrollback, feed}` are `pub(crate)` in `tear-core`, so the only way to advance a pane's state from outside the crate is through `InProcess`/the daemon. Reading (`snapshot()`, `PaneSnapshot`, `Cell`) stays public — **anyone may read, only the authority may advance.** | truly-unrep |
| two grids disagree on a pane's contents | there is only one grid, and no consumer can construct a second — conditional on the row above, and now graded with it | truly-unrep |
| a *borrowed* view outlives its grid | `PaneView<'a>` borrows the grid's interior — borrowck | truly-unrep |
| acting on a **stale wire** view | `epoch: GridEpoch` on `OwnedPaneView`. **Ceiling: nothing forces the comparison — it is a runtime check the client chooses to make. Does NOT apply to the borrowed carrier; do not let the borrowed row's green cover this one.** | only-mitigated (C3) |
| the renderer reads a mode from its own dead parser | the seven accessors are **deleted** from `Terminal` *and* from `TerminalOps` (five have two names each), so every missed site is `E0599` — repointing while leaving the old method alive is not sufficient | truly-unrep |
| a mode is substituted for another mode | ten distinct newtypes, not ten `bool`s — `sanitize_paste(text, BracketedPaste)` is `E0308` against any other mode. This is a **paste-injection** surface, not a cosmetic one | truly-unrep |
| modes read from a different instant than the cells | `ModeSet` is a **field of `PaneView`**, never a separate request — no `ModeSet` exists without the view it came from | truly-unrep |
| **graphics silently dropped at the wire** | ~~a **non-Option** field~~ — **over-graded.** A `#[serde(default)]` empty vec and an absent field are **the same bit pattern**; the type alone cannot tell them apart. Needs a `view-graphics` capability gating a `Subscription<WithGraphics>` typestate | parse-time-rejected |
| a capability-gated field read on a daemon that never advertised it | `require()` called by convention per site. **Ceiling: depends on every call site remembering, and no test enumerates them.** Earns parse-time-rejected only when the capability becomes a typestate precondition of the subscription | only-mitigated (C4) |
| **a program's DSR/DA/DECRQSS probe is never answered** | **NOT CORNERED — and this is a flip blocker nobody had listed.** mado answers CPR/DA/DECRQSS from its own parser via `take_response`; under shuken mado has no parser, and tear owns the PTY but has **no response state whatsoever**. Every app that probes the terminal (prompt libraries using CPR, DA-based capability detection) hangs waiting for a reply that no longer exists. **Ceiling: nothing exists; the failure mode is a hung child process.** | only-mitigated (C6) |
| scroll position diverges between two windows on one pane | scroll stays **renderer-side** in a mado-owned viewport and is never a view field — see §5-B | truly-unrep (the field does not exist on the shared type) |
| geometry the daemon believes ≠ geometry drawn | both derive from one `compute_rects` over one `LayoutNode` | truly-unrep |
| a pane id with no live PTY | kernel-process liveness, same class as SESSION-TYPESCAPE §7 #7 | only-mitigated (C2) |

**A row may not be written green here without a red run against a
deliberately broken input.** Two rows were green on this table for the length
of one afternoon on nothing but plausibility; that is the exact failure this
sentence exists to prevent, and it caught nobody — a later measurement did.

## 5-A. ★ PRECONDITION — parser parity before the flip

**On the wide-character axis, mado is the CORRECT parser and tear is the
wrong one.** Flipping authority to `PaneGrid` as it stands today would make
the symptom that started this work *worse*.

- `tear-core/src/pane_grid.rs:236-244` — `advance_cursor_after_print` is
  `self.cursor_col += 1`, unconditionally. tear has **no `unicode-width`
  dependency at all**.
- `mado/Cargo.toml:313` declares `unicode-width`; `mado/src/terminal.rs:390-391`
  gives `Cell` a `width: u8` with `0 = continuation of a wide char`.
- Combining characters: mado has `Cell.extra: Option<Box<Vec<char>>>`
  (terminal.rs:387); tear's `Cell` (pane_snapshot.rs:151) has no slot, so
  each combining codepoint takes its own cell and advances.

`PaneGrid` must reach parity on both axes **before** it becomes
authoritative. This is a precondition, not a follow-up — the full Gate 0 and
the measurement behind it are in
[`theory/MADO-TEAR-SEAM.md`](https://github.com/pleme-io/theory/blob/main/MADO-TEAR-SEAM.md)
§III-A.

## 5-B. ★ The border is VIEW vs VIEWPORT — not "everything in `Terminal` moves"

The largest structural finding of the design pass, and it is not in the
original decision. Measured: `scroll_up` / `scroll_down` / `scroll_to_bottom`
/ `scroll_offset` are **46 combined call sites**, they are **mutations**, and
they are **not VT state**. Scroll position is what the operator's eyes are
looking at. Move it into the authority and two mado windows attached to one
pane fight over the scrollback position.

> **The rule that generates the border:**
> **everything the BYTE STREAM determines moves to the authority; everything
> the OPERATOR'S EYES determine stays with the renderer.**

- **Moves (view fields):** cells, cursor, styles, links, palette, graphics,
  modes, title, cwd, prompt blocks, scrollback *content*, response bytes.
- **Stays (a mado-owned viewport):** `scroll_offset`, selection anchors,
  search state + matches, URL detection, font zoom, kinetic scroll. Each is a
  **pure function of the view plus operator input** — `resolve_selection_span`,
  `detect_urls`, `search_rows` all just read cells. **None needs a parser.**

Two consequences worth having up front:

1. **The move is ~2,150 lines smaller than §6.2 assumed** — and §6.2's own
   number was inflated anyway: `terminal.rs` is 11,736 lines but `#[cfg(test)]`
   opens at 6158, so only **~6,150 lines are production code**. The parser
   surface to move is ~4,000, not 11,736.
2. **mado has already discovered this type internally.** `render.rs:3298`'s
   `fn snapshot(&self) -> (Snapshot, u64)` is *exactly* `PaneView`'s shape —
   cloned cells, style snapshot, palette, cursor, image placements, no parser.
   shuken makes that existing internal boundary the cross-process one, which
   is why this is a re-homing rather than an invention.

## 6. What the decision forces (in scope, not optional)

1. **The wire carries graphics.** A pane snapshot must convey image
   placements and interned style, which SESSION-TYPESCAPE §7 previously
   listed as unresolved. Whether payloads cross by value or as handles
   into a daemon-side store is a live perf question, not a detail — a
   by-value sixel frame on every snapshot is a different system than a
   handle.
2. **mado's `terminal.rs` splits.** 11,736 lines, of which the parser
   retires and the reflow / search / selection / scrollback readers become
   view methods. The size of that split decides whether this is one phase
   or three.
3. **Every reader repoints.** `render.rs` and `ux::engine` read the grid
   directly today; both move to `PaneView`.

## 7. What this is *not*

- Not a claim that any of it is built. See the status block.
- Not a graphics/text partition — that option was considered and rejected
  in §1.
- Not a reason to touch `Durability`. Restart-survival remains Phase-6 and
  is orthogonal: shuken is about *who owns live state*, not *whether it
  survives a restart*.

## 8. Provenance

Operator decision 2026-07-31, arising from a comparison of mado + tear
against Ghostty 1.3.1 and Superlogical (announced 2026-07-29). Superlogical's
stated architectural claim — that legacy multiplexers *"sit between your
terminal emulator and the pty, causing double parsing and duplicated state
processing"* — names precisely the debt this repo's own M5 row already
carried. Its consequence, that *"every connecting client [must] be a very
smart, high-functioning, compliant client"*, is a **requirement** in their
design; under shuken it is instead a **construction** — a non-compliant
client is unconstructible, because a client with no parser cannot diverge
from the authority.
