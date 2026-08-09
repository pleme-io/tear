# praça — the session-orchestration platform

> *praça* (Brazilian-Portuguese, "town square / commons"): the place where
> everything gathers, where you arrive, find your way by reflex, and move on.
> Sessions are **places you inhabit**; praça is the typed layer that makes
> finding, entering, creating, and organizing them muscle-memory.

> **Typed model + tier ledger:** [`SESSION-TYPESCAPE.md`](./SESSION-TYPESCAPE.md)
> is the canonical type model (definition↔instance, model↔view, the
> illegal-state ledger, the Shipped/M2/M5/Phase-6 tiers). Read it for
> *what is true by construction*; read this file for *how praça uses it*.

praça sits **on top of tear** (which owns the live session/pane model via
`MultiplexerControl`), surfaces **through mado** (UI + keymap), takes naming
+ keybinds from **ishou** (`FleetSessionNames`, `FleetKeybinds`), and authors
templates in **tatara-lisp** (`(defsession …)`). It is a crate in the tear
workspace — `praca` — plus thin integration in tear-daemon (persistence) and
mado (auto-attach + picker).

## The chosen mode — automation-first, fewest keys

The operator picked **automation-first**: sessions are auto-named and
auto-bound to projects; `cd` into a project auto-attaches that project's
session; the picker is the rare fallback you reach for to *browse*.

```
cd ~/code/.../mado   → auto-attach 🌊 tide   (the mado project's session)
cd ~/code/.../nix    → auto-attach ❄ frost
(new project)        → auto-spawn + auto-name (deterministic from the root)
Ctrl-S               → fuzzy browse, only when you want to look around
```

The magic is a **stable project → name** map: `FleetSessionNames::from_project_path`
hashes the project root with a run-stable FNV-1a seed, so `~/.../mado` is
**always** `🌊 tide` across daemon restarts. A project root is the nearest
ancestor with a marker (`.git`, then `Cargo.toml`/`flake.nix`/`package.json`/…).

## The typed substrate (`praca` crate — pure, tested, time-injected)

| Type | Role |
|---|---|
| `SessionRecord` | a tear session + praça metadata: name, `project_root`, cwd, `visits`, `last_seen`, tags, state (`Live`/`Saved`/`Templated`) |
| `ProjectBinding` | serde-persisted `project_root → SessionId` map (the auto-attach memory) |
| `SessionIndex` | searchable/ranked collection — fuzzy match on name-word/cwd/tags, ranked by frecency |
| `frecency::score` | recency-weighted frequency — `visits × decay(age of the last visit)`. The decay curve is **not defined here**: it is `wadachi_spec::DecayKind::ZoxideLogBuckets`, the fleet frecency primitive. praça held a byte-identical private copy of its thresholds and multipliers until 2026-08-09. The configuration praça uses ships upstream as the named instance `praca-parity` |
| `AttachDecision` / `AttachPolicy` | the automation core: `Stay` / `SwitchTo(id)` / `SpawnNew{root,name}`; policy `AutoSwitch`/`SuggestOnly`/`PickerOnly` |
| `Praca` facade | `on_cwd_change(current, new_cwd, now) → AttachDecision`, `record_visit`, `search` |

All time is an injected `u64` (no `SystemTime::now()` in the substrate →
deterministic + testable). The facade is what mado/tear call.

## The flow (M0)

```
frost emits OSC-7 cwd  ─→  mado terminal tracks focused-session cwd
        │
        ▼  on cwd change
  Praca::on_cwd_change(current_session, new_cwd, now)
        │
        ├─ same project root  → Stay
        ├─ bound elsewhere    → SwitchTo(id)      → mado attaches that session
        └─ new project        → SpawnNew{root,name}→ tear new_session(name) + bind
        │
        ▼  (persisted)        tear-daemon writes ProjectBinding + frecency
  Ctrl-S  → mado picker overlay → Praca::search("") → frecency-ranked list → switch
```

## Phased roadmap (destination → steps)

- **M0 · Navigate** — `praca` substrate + project→name + auto-attach on cd +
  emoji naming + the Ctrl-S picker. *Find/enter any session with ≈ zero keys.*
- **M1 · Remember** — tear-daemon persistence (bindings + frecency survive
  restarts), tags + groups.
- **M2 · Template** — `(defsession …)` / `(defplanta …)` blueprints (cwd,
  panes, layout, commands, env); new-from-template; composable configs.
- **M3 · Organize / Design** — projects, a session-design surface,
  import/export/share.
- **M4 · Keymap platform** — shikumi-typed keymap *styles* (picker /
  vim-modal / tmux-leader); every op a typed `FleetKeybinds` intent.

## Proof discipline

Each phase ships a proof pass: substrate has exhaustive unit tests (project
walk-up, frecency order, index rank, every `AttachDecision` branch);
integration is proven through mado's MCP — spawn N sessions across project
dirs, assert the right session auto-attaches on cd and the picker finds +
switches. The same loop we used for the render fixes (spawn → snapshot/query →
assert).

## Reuse, not reinvention

praça is mostly *wiring existing primitives*: tear (sessions),
mado OSC-7 cwd + recent-dirs + MCP session control, wadachi (dir frecency
pattern), ishou `FleetSessionNames` (naming) + `FleetKeybinds`, skim-tab
(fuzzy picker). The new parts are the project↔session binding, the
auto-attach policy, and the deterministic naming — all in the `praca` crate.
