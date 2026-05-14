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
| Pane domain | `mado::pane::*` (legacy; tier-3 rebases onto tear) | `tear-types::{TearPane, TearWindow, TearSession, LayoutNode}` | `tear-types` — pure typed surface, no I/O. |
| Pane state machine | `mado::tab::*` (legacy) | `tear_core::InProcess` (canonical) | At M5 mado rebases its tab/pane modules onto `InProcess`. |
| Multiplexer trait | `kaname` MCP tools call → `tear_client::Client` | `tear_types::MultiplexerControl` (the universal trait) | The trait is the wire format between mado-as-driver and tear-as-multiplexer. |

**Result:** mado and tear are not "two separate apps that happen to
work together" — they're two views over a single typed substrate.
Adding a new pane operation lands in one place (`tear-types::MultiplexerControl`
+ `tear-core::InProcess`); both apps get it.

## Workspace shape

| Crate | Role | M0 Status |
|---|---|---|
| `tear-types` | Pure typed domain — `SessionId`/`WindowId`/`PaneId` (BLAKE3-derived), `TearSession`/`TearWindow`/`TearPane`/`LayoutNode`/`KeyTable`/`StatusBar`/`TearTheme`, the `MultiplexerControl` trait. No I/O. | **shipped** |
| `tear-config` | Shikumi-style live config — `~/.config/tear/tear.yaml` parser, `LiveConfig` w/ `ArcSwap<TearConfig>`, hot-reload via `notify`. Same pattern mado uses. | **shipped** |
| `tear-core` | Runtime: `InProcess` impl of `MultiplexerControl`, `Registry` typed state, `PtyHandle` (via `portable-pty`). | **shipped** (M0 minimum-viable; M2 wires real layout + per-pane vte parsing) |
| `tear-daemon` | Long-running server. Owns sessions across client disconnects. UDS RPC. Wraps `tear-core::InProcess`. | scaffold (M2) |
| `tear-client` | Typed RPC client. Consumed by the `tear` bin, by mado at Tier 2, by remote operators over SSH. Implements `MultiplexerControl` so remote ≡ local from the consumer's perspective. | scaffold (M2) |
| `tear-tmux-backend` | Renders a typed `TearConfig` → `tmux.conf`. The M0 path AND the permanent escape hatch for remote hosts that have tmux but not tear. | **shipped** |
| `tear` (bin) | Multi-call CLI: `up`/`list`/`render`/`config-check`/`config-path`. GH releases via `rust-workspace-release-flake.nix`. | **shipped** |

## Build & run

```bash
cargo check --workspace          # whole tree
cargo test  --workspace          # 20+ tests across crates
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
- **M2** — `tear-daemon` + `tear-client` UDS RPC. `tear list` / `tear attach` route through the daemon. Persistence across client detach.
- **M3** — tmux `.tmux.conf` parser + format-string evaluator → existing tmux configs drop in.
- **M4** — mado integration: `kaname` MCP tools route through `tear-client`. mado can spawn / split / kill panes in a tear-daemon-owned session over UDS.
- **M5** — mado `pane.rs` / `tab.rs` rewritten on top of `tear_core::InProcess`. The substrate has one source of truth for pane semantics: tear is the single owner of the typed pane state machine.

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
