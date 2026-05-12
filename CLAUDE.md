# tear — Claude Orientation

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
windows, and sessions into a working fabric. Composes with mado at
Tier 2 (GPU-native splits) and with stock Ghostty / iTerm2 / WezTerm /
xterm at Tier 1 (`tmux -CC` control-mode protocol).

## Workspace shape

| Crate | Role | Status |
|---|---|---|
| `tear-types` | Pure types — `TearSession`/`Window`/`Pane`/`Layout`/`KeyTable`/`Hook`/`StatusBar`/`Theme`, the `MultiplexerControl` trait. No I/O. crates.io. | scaffold |
| `tear-core` | Session/window/pane state machine, PTY (via `portable-pty`), layout, tmux.conf parser, format-string evaluator. The `InProcess` `MultiplexerControl` impl. crates.io. | scaffold |
| `tear-daemon` | Long-running server. Owns sessions across client disconnects. UDS RPC. Wraps `tear-core`. crates.io. | scaffold |
| `tear-client` | Typed RPC client. Consumed by the `tear` bin, by mado at Tier 2, by remote operators over SSH. crates.io. | scaffold |
| `tear-tmux-backend` | Renders a typed `TearProfile` → `tmux.conf`, shells out to vanilla tmux. M0 path. Permanent escape hatch for remote hosts that only have tmux. | scaffold |
| `tear` | Multi-call CLI: `up`/`attach`/`snapshot`/`restore`/`render`. GH releases via `rust-workspace-release-flake.nix`. | scaffold |

## Build & run

```bash
cargo check                # workspace-wide
nix run .#tear -- --help   # via substrate's workspace builder
```

## Where to look

| Intent | File |
|---|---|
| Workspace + members | `Cargo.toml` |
| Substrate builder wiring | `flake.nix` (consumes `substrate/lib/rust-workspace-release-flake.nix`) |
| Repo-forge spec (regenerate boilerplate) | `repo-forge.lisp` |

## Project plan

- **M0** (weeks 1-4): `tear-types` + `tear-core` skeleton + `tear-tmux-backend` rendering typed profile → `tmux.conf`; replace blackmatter-shell's dormant tmux module.
- **M1-M2** (weeks 5-8): `tear-daemon` + `tear-client` UDS IPC.
- **M3** (weeks 9-12): tmux `.tmux.conf` parser + format-string evaluator → existing tmux configs drop in.
- **M4** (weeks 13-16): mado integration — `kaname` MCP tools route through `tear-client`.
- **M5** (weeks 17-24): mado `pane.rs`/`tab.rs` rewritten on top of `tear-core::InProcess`. Substrate has one source of truth for pane semantics.

## Constraint

Tmux backend stays a permanent escape hatch, not a transitional scaffold —
remote hosts that have tmux but not tear must remain operable.
