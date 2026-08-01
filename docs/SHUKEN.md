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

The seal that closes that is dependency absence: **`vte` is removed from
mado's manifest.** A transitive dependency is not nameable without being
declared, so `vte::Parser` in mado becomes **`E0433` unresolved crate** —
a compile error, at the strongest tier available, for the accidental case.

Grade this honestly (§5): re-adding a dependency is a deliberate,
reviewable act, and a forcing-function test asserting `vte ∉ mado`'s
manifest is *only-mitigated* against someone choosing to. The
**accidental** regrowth of a second parser is truly-unrepresentable; the
**deliberate** one is mitigated by a gate that has to actually run — and
mado has no test CI today, which is a named precondition of this work, not
a footnote.

## 5. Target tier ledger

<!-- tier-ledger -->

| bad state | how the vocabulary corners it | tier (TARGET) |
|---|---|---|
| a renderer advances VT state | `PaneView` exposes no byte-feeding verb — `E0599` | truly-unrep |
| mado grows a second parser by accident | `vte` absent from mado's manifest — `E0433` unresolved crate | truly-unrep |
| mado grows a second parser deliberately | forcing-function test asserting the manifest; **ceiling: requires a CI gate mado does not yet have** | only-mitigated (C4) |
| two grids disagree on a pane's contents | there is only one grid; the second is deleted, not synchronized | truly-unrep |
| a view outlives the grid it borrows | `PaneView<'a>` borrows — the borrow checker refuses | truly-unrep |
| graphics silently dropped at the wire | the snapshot carries placements as a **non-Option** field | truly-unrep |
| geometry the daemon believes ≠ geometry drawn | both derive from one `compute_rects` over one `LayoutNode` | truly-unrep |
| a pane id with no live PTY | *pending recon* — expected **C2 ceiling** (kernel-process liveness, same class as SESSION-TYPESCAPE §7 #7) | only-mitigated (C2) |

Rows marked *pending recon* are placeholders. **A row may not be written
green here without a red run against a deliberately broken input.**

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
