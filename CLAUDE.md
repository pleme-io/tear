# tear — Claude Orientation

pending-quadro: `tear/src/top.rs` depends on **ratatui directly** (3 use-sites;
`tear/Cargo.toml` pins `ratatui = "0.30"`), which deviates from QUADRO's "widget
logic lands in egaku, never in ratatui". Declared 2026-08-01 rather than fixed,
with the trade-off stated so the next reader can re-decide instead of
re-discovering: the compliant form is `moldura::ratatui` (moldura is a facade
re-exporting ratatui/crossterm/egaku/egaku-term/shikumi, and its ratatui is the
same 0.30), but adopting it pulls the whole TUI stack into the **multiplexer
daemon** repo to serve one small `tear top` dashboard. That cost is plausibly
worse than the deviation, so this is a real decision and not an oversight.

**What makes it a defect today is that it was UNDECLARED**, not that it exists —
an undeclared deviation is invisible to an audit, a declared one is a row.
Revisit if `tear top` grows real widget logic, at which point the dependency
earns its keep and the trade-off flips.

> **★★★ CSE / Knowable Construction.** This repo operates under
> **Constructive Substrate Engineering** — canonical specification at
> [`pleme-io/theory/CONSTRUCTIVE-SUBSTRATE-ENGINEERING.md`](https://github.com/pleme-io/theory/blob/main/CONSTRUCTIVE-SUBSTRATE-ENGINEERING.md).
> The Compounding Directive (operational rules: solve once,
> load-bearing fixes only, idiom-first, models stay current, direction
> beats velocity) is in the org-level
> [`pleme-io/CLAUDE.md`](https://github.com/pleme-io/pleme-io/blob/main/CLAUDE.md)
> ★★★ section. Read both before non-trivial changes.

One-sentence purpose: Rust-native tmux-compatible terminal multiplexer
with typed shikumi config — Portuguese *tear* (loom) weaves panes,
windows, and sessions into a working fabric. **Built explicitly to
pair with mado** (the pleme-io GPU terminal emulator); the two apps
share shikumi-style configuration, the typed pane domain
(`tear-types`), and an eventual common pane state-machine
(`tear-core::InProcess`) so a fleet operator learns ONE mental model
that covers both layers.

## tear ↔ mado pairing

The two repos are coupled by design. The compounding shape:

| Layer | mado | tear | Shared via |
|---|---|---|---|
| Configuration | `~/.config/mado/mado.yaml` | `~/.config/tear/tear.yaml` | **shikumi** + the same hot-reload pattern (`ArcSwap<Cfg>` + `notify`). Operators learn one config style. |
| Theme / typography | `ishou-tokens::MonoFonts::pleme()` | `tear-types::TearTheme::nord()` (defaults to ishou's Nord palette) | **ishou-tokens** — one palette + typography surface fleet-wide. |
| Allocator | `#[global_allocator] mimalloc` | `#[global_allocator] mimalloc` | Same crate. |
| Lock primitive | `parking_lot::RwLock<Terminal>` | `parking_lot::RwLock<Registry>` | Same crate. |
| PTY library | (transitive) `portable-pty` | `portable-pty` directly via `tear-core::pty::PtyHandle` | Same crate. |
| Pane domain | **none — deleted at Phase 4** | `tear-types::{TearPane, TearWindow, TearSession, LayoutNode}` | `tear-types` — pure typed surface, no I/O. |
| Pane state machine | **none — deleted at Phase 4** | `tear_core::InProcess` (canonical) | mado links `tear-core` and runs `InProcess` **in-process** by default (no daemon, no socket). |
| Multiplexer trait | mado's **rmcp** MCP server (18 `tear_*` tools) → `tear_client::Client` | `tear_types::MultiplexerControl` (the universal trait) | The trait is the wire format between mado-as-driver and tear-as-multiplexer. |

**Result:** mado and tear are not "two separate apps that happen to
work together" — they're two views over a single typed substrate.
Adding a new pane operation lands in one place (`tear-types::MultiplexerControl`
+ `tear-core::InProcess`); both apps get it.

## Workspace shape

| Crate | Role | M0 Status |
|---|---|---|
| `tear-types` | Pure typed domain — `SessionId`/`WindowId`/`PaneId` (BLAKE3-derived, truncated to 8 bytes: a spawn-unique handle, never an identity), `Genesis`/`Guid` (full 256-bit, the unforgeable identity), `Address`/`address::Segment`/`Pattern` (the mutable dot-separated alias; not wired to any lookup yet), `TearSession`/`TearWindow`/`TearPane`/`LayoutNode`/`KeyTable`/`StatusBar`/`TearTheme`, the `MultiplexerControl` trait. No I/O. | **shipped** |
| `tear-config` | Shikumi-style live config — `~/.config/tear/tear.yaml` parser, `LiveConfig` w/ `ArcSwap<TearConfig>`, hot-reload via `notify`. Same pattern mado uses. | **shipped** |
| `tear-core` | Runtime: `InProcess` impl of `MultiplexerControl`, `Registry` typed state, `PtyHandle` (via `portable-pty`). | **shipped** (M0 minimum-viable; M2 wires real layout + per-pane vte parsing) |
| `tear-daemon` | Long-running server. Owns sessions across client disconnects. Length-prefixed CBOR over UDS (or `--tcp`). Wraps `tear-core::InProcess`. | **shipped + LIVE** (3,192 LOC; runs as a user launchd/systemd unit from a Nix store path) |
| `tear-client` | Typed RPC client. Consumed by the `tear` bin, by mado, by remote operators over SSH. Implements `MultiplexerControl` so remote ≡ local from the consumer's perspective. | **shipped** (2,476 LOC) |
| `tear-tmux-backend` | Renders a typed `TearConfig` → `tmux.conf`. The M0 path AND the permanent escape hatch for remote hosts that have tmux but not tear. | **shipped** |
| `praca` | Session orchestration: project-root→emoji-name hashing, frecency, project↔session bindings, cd-driven attach, the definition↔instance algebra. Time is injected — never reads a clock. | **shipped** (3,915 LOC) |
| `tear-ws-bridge` | Re-frames the same CBOR wire over WebSocket. | **shipped** (395 LOC) |
| `mado-web` | wasm32 browser client. **Skeleton** — streams raw bytes into a `<pre>`; no cell grid, no glyph atlas, no input path. Out-of-workspace, own `Cargo.lock`. | skeleton |
| `tear` (bin) | Multi-call CLI, ~24 subcommands: `up`/`list`/`kill`/`rename`/`attach`/`top`/`mcp`/`daemon`/`blocks`/`block`/`history`/`replay`/`audit`/`ai`/`snapshot`/`migrate`/`pane-input`/`pane-info`/`pane-record`/`render`/`status`/`config-*`. GH releases via `rust-workspace-release-flake.nix`. | **shipped** |

> **★ `attach` does NOT render.** It connects, prints the session list, and
> exits — and it silently discards its target argument (`let _ = target;`).
> **There is no interactive attach client and none is planned as a tear
> artifact.** Rendering is mado's job, or real tmux's via
> `tear render --backend tmux`. Do not read "shipped" above as "usable as a
> tmux replacement inside another terminal" — it is not.

## Build & run

```bash
cargo check --workspace          # whole tree
cargo test  --workspace          # 524 #[test]/#[tokio::test] attributes
                                 # NOTE: no automated green receipt — ci.yml is
                                 # disabled_manually AND repo Actions are off in
                                 # org.yaml. Its history is 76 runs / 76 failures
                                 # / 0 successes. Run it locally; do not assume.
nix run .#tear -- --help         # via substrate's workspace builder

# Live shikumi config — same pattern as mado
$EDITOR ~/.config/tear/tear.yaml
tear config-check                # validate the YAML
tear render --backend tmux       # render to tmux.conf for tier-1 hosts
tear render --backend yaml       # round-trip back to YAML (debug)
tear up                          # M0 — create an in-process session
```

## Live configuration

Tear ships **shikumi-style live configuration** identical in shape
to mado's:

1. Operator edits `~/.config/tear/tear.yaml`.
2. `tear_config::LiveConfig` wraps `ArcSwap<Arc<TearConfig>>` for
   lock-free reads.
3. `LiveConfig::spawn_watcher()` arms a `notify` watcher on the
   config directory; file changes (debounced) trigger
   `LiveConfig::reload()` which atomically swaps the Arc.
4. Readers (the bound-key dispatcher, the status-bar refresher,
   the layout engine) call `live.load()` to get an
   `Arc<TearConfig>` they can hold across a frame — no lock
   contention with the watcher's swap.

The exact same shape ships in mado for `mado.yaml`; operators
authoring either learn one mental model.

## Where to look

| Intent | File |
|---|---|
| Workspace + shared deps (pin once, used fleet-wide) | `Cargo.toml` |
| Typed pane domain (the bit mado consumes) | `tear-types/src/{session,window,pane,layout,direction,control,id}.rs` |
| Live shikumi config | `tear-config/src/lib.rs` |
| InProcess state machine + PTY pump | `tear-core/src/{inproc,pty,registry}.rs` |
| tmux.conf rendering | `tear-tmux-backend/src/lib.rs` |
| CLI entry point | `tear/src/main.rs` |
| Substrate builder wiring | `flake.nix` (consumes `substrate/lib/rust-workspace-release-flake.nix`) |
| Repo-forge spec (regenerate boilerplate) | `repo-forge.lisp` |

## Project plan (updated)

- **M0** ✅ — `tear-types` + `tear-config` + `tear-core` + `tear-tmux-backend` + `tear` CLI all shipped. Workspace compiles + tests green. Replace blackmatter-shell's dormant tmux module by sourcing `tear render --backend tmux`.
- **M1** — Status-bar refresh loop on a tokio task; expose `TearTheme` resolution against ishou-tokens runtime.
- **M2** ✅ — `tear-daemon` + `tear-client` CBOR-over-UDS RPC shipped and live; sessions persist across client detach (`Durability::ProcessBound` — a session outlives its client, but **not** a daemon restart).
- **M3** — tmux `.tmux.conf` parser + format-string evaluator → existing tmux configs drop in. **Zero code today**; the tmux relationship is one-way (tear config → tmux.conf), never the reverse.
- **M4** ✅ — mado integration: mado's rmcp server exposes 18 `tear_*` tools; mado can spawn / split / kill panes. Default runtime is **embedded `InProcess`**, not the daemon.
- **M5** — the no-overlap endpoint, and the live work: **(a)** mado renders multi-pane from `compute_rects` — the model is correct and nothing draws it, because `render_multi_pane` was deleted at Phase 4 (the clipping primitive it needed **now exists** — garasu `51db8ce`+`4883ee7`, 2026-07-31, shipped `PaneRect`/`LayeredPass::in_pane`/`PanePass` — and has **zero consumers fleet-wide**, measured 2026-08-03); **(b)** retire mado's `terminal.rs` double-parse so the session model lives once. **The direction is decided** — see [`docs/SHUKEN.md`](./docs/SHUKEN.md): `PaneGrid` becomes the sole VT authority and the seam is typed as *ownership*, not content. **Precondition — SUPERSEDED 2026-08-03, do not re-cite.** This read: "`PaneGrid` must first reach mado's correctness on wide chars + combining characters — today it has no `unicode-width` dependency and advances the cursor by 1 unconditionally." Both halves are now false: `tear-core/src/pane_grid.rs:28` imports `UnicodeWidthChar`, the width is read at `:1015`, combining marks attach at `:248-250` (`tear@357e718`), and [`docs/SHUKEN.md`](./docs/SHUKEN.md):227 grades Gate-A parser parity **CLEARED** along with its two sibling blockers. The remaining M5(b) blocker is **OSC 8 hyperlinks + a link table** (SHUKEN.md:239-242), not parser parity. Note SHUKEN.md:255-259 still binds: a half-migrated mado is *strictly worse than either endpoint*, so the flip is one commit or none.

> **Corrected 2026-07-31.** This file had gone ~2.5 months stale (last
> substantive touch 2026-05-14) and described `tear-daemon`/`tear-client` as
> "scaffold (M2)" while both were shipped and the daemon was running. It also
> predated `praca`, `tear-ws-bridge`, the MCP server, blocks, recording, audit,
> capability negotiation, `tear top`, `tear ai`, and the `pleme-tear` crates.io
> rename. **Treat `docs/SESSION-TYPESCAPE.md` + `docs/SESSION-FEATURESET.md`
> (dated 2026-06-23, and carrying real tier ledgers) as authoritative over this
> file** wherever they disagree.

## Constraints

- **Tmux backend is permanent**, not transitional — remote hosts that
  have tmux but not tear must remain operable indefinitely.
- **`tear-types` carries no runtime deps** beyond `serde` + `blake3` +
  `thiserror` + `anyhow`. Third-party drivers should be able to
  implement `MultiplexerControl` without pulling tokio / portable-pty
  / etc into their tree.
- **Shikumi config style is shared with mado** — never introduce a
  tear-specific config dialect that mado can't read or vice versa. If
  a new config knob makes sense on both apps, name + shape it the
  same way in both.
