# The Session Featureset — projections of the typescape

> **Companion to [`SESSION-TYPESCAPE.md`](./SESSION-TYPESCAPE.md).** That
> doc is the typed *model* (what is true by construction). This doc is the
> user-facing *featureset* derived from it: every operator capability is a
> thin projection of a shipped typed primitive, the whole thing extends
> today's Ctrl-S + cd-auto-attach (never replaces it), and every feature
> is graded `buildable-now / needs-M2 / needs-M5` — never rounded up.

The original ask was a unified theory of "running multiple mado instances,
different sessions, session presets, navigating presets instantiated and
not, split-screens as layouts, persistence across all this — gracefully
extending what we currently do." This is that, re-derived from the
typescape so each feature *flows from the model* instead of being bolted
on. Designed via a 5-lens design panel → adversarial synthesis; verified
against the shipped code before adoption.

---

## 1. The unified operator surface — Ctrl-S is the one union navigator

There is **one** entry point. `Ctrl-S` (`Action::SessionPickerOpen`,
unchanged keybind) opens a single frecency+fuzzy-ranked space over the
disjoint union:

```
{ live instances    → Switch }                  ●live
∪ { latent presets (no live instance) → Instantiate }   ○latent
∪ { emoji / typed create → Create }
```

One ranked list spanning *what is running* and *what could run*. Typing
`wave` surfaces `🌊 tide` through the shipped keyword tier (emoji-native).
`cd`-auto-attach stays the default; Ctrl-S stays its fallback.

Every session capability lands as **exactly one** `Action` variant (mado)
+ one `RowKind` arm (praça dispatch) + one tear method (model) + at most
one mado-MCP write tool. The verb set:

| Verb | Layer | Derives from | Gesture |
|---|---|---|---|
| open union navigator | mado-view | `Action::SessionPickerOpen` + `Overlay` FSM | `Ctrl-S` |
| accept (Switch \| Instantiate \| Create) | praça | `AttachAction` 3-way | `Enter` |
| new instance of this def | praça | `instantiate(def, &dyn MultiplexerControl)` | `Ctrl-Enter` |
| expand / collapse a def header | mado-view | `FuzzyPicker<Row>` selection | `→`/`Tab` · `←`/`Esc` |
| save focused session as preset | praça | `SessionDefinition::single_pane` | `Ctrl-S` then `s` |
| split focused pane | tear-model | `split_pane → LayoutNode::split_leaf` | `Ctrl-W v`/`s` |
| navigate panes directionally | tear-model | `LayoutNode::neighbor` | `Ctrl-h/j/k/l` |
| resize focused pane | tear-model | `LayoutNode::resize_leaf` | `Ctrl-W ⇧H/J/K/L` |
| close focused pane | tear-model | `LayoutNode::remove_leaf` | `Ctrl-W q` |
| apply named layout preset | tear-model | `LayoutKind::from_kind` | `Ctrl-W Space` / `1`–`5` |
| read window layout | tear-model (read) | `compute_rects` via `get_window` | MCP `get_window_layout` |
| list sessions (read projection) | praça | per-def name + root + live count | MCP `list_sessions` |
| instantiate / switch / spawn / close | praça | the four `AttachAction` arms | MCP write verbs |

### The no-overlap law (the bright line)

A session model lives **once**, in tear. mado is a pure view of it. praça
is the typed map between a preset and its instances.

- **tear (model)** owns the single source of truth: one `LayoutNode` per
  window (`split_leaf`/`resize_leaf`/`neighbor`/`remove_leaf`/`compute_rects`)
  that both sizes PTYs and *is* the only layout model. **tear-MCP is
  read-only** (`get_window_layout`, `list_panes`, `pane_snapshot_text`,
  `session_detail`).
- **mado (view/input)** owns pixels, the modal `Overlay` FSM, the
  `FuzzyPicker<Row>` selection, the keyboard. It draws `compute_rects`
  output + plain `RowKind` labels — never a second session/layout model,
  never a second decision enum. mado-MCP owns the **write** verbs.
- **praça (orchestration)** owns the latent↔live algebra
  (`SessionDefinition`/`InstanceRegistry`/`instantiate`/`AttachAction`) +
  the accept dispatch — it decides *which* model action a gesture maps to,
  but never renders and never owns PTYs.

The moment a capability appears as a bespoke mado mode, a second layout
model, or a second decision enum, the surface has regressed into the fork
the typescape exists to delete.

---

## 2. Graceful extension — three non-breaking waves

The migration is purely additive over today's two-gesture posture
(`cd` = default auto-attach, `Ctrl-S` = fallback switch picker). The
keybind and the switch channel are never rewritten.

- **Wave 0 — buildable-now (the first brick + siblings).** `from_kind`,
  the `Searchable` ranking seam, single-pane `save-as-preset` (capture the
  focused live session's cwd+shell into a `SessionDefinition::single_pane`
  held in an **additive in-mado catalog — not `PracaSnapshot`**), and a
  read-only `get_window_layout` MCP tool. Ctrl-S and cd behave
  byte-identically; the operator gains an MCP read surface, a save verb,
  and named-layout authoring-as-data. **Nothing on disk changes.**
- **Wave 1 — additive read-model bridge.** mado carries an
  `InstanceRegistry` as an additive read-model *derived* from its existing
  live `ProjectBinding` + `first_pane_of` (def-for = project-root key
  until M2), so the picker `list()` groups existing `Switch` rows under a
  definition header and appends latent-preset rows from the in-mado
  catalog — a new `RowKind::Instantiate(DefinitionId)` arm + a `○latent /
  ●live(≥1)` badge (**honestly not an exact ●N** — that needs the registry
  as source of truth, M2). `decide()` is untouched; the Instantiate arm
  calls the shipped `instantiate()` through the same `spawn→first_pane→
  switch.post` pipeline `create_and_switch` already uses.
- **Wave 2 — the M2 flip (the only breaking step, staged behind
  backward-compat deserialization).** `decide()→AttachAction`;
  `ProjectBinding→InstanceRegistry` as source of truth; `PracaSnapshot`
  grows `#[serde(default)] definitions`; `SessionRecord` becomes a
  projected view; mado routes through the one daemon `PracaStore`. Now cd
  auto-instantiates a cold preset, exact ●N badges become real, and
  save-as-preset persists across restart. Because Waves 0–1 already
  delivered the operator-visible payoff through additive read-models,
  Wave 2 is a **source-of-truth swap the operator barely notices** — not a
  surface rewrite.

---

## 3. The tier partition

### Buildable-now (Wave 0 — shipped additive types, zero live state)

- **`from_kind`** — `LayoutKind::from_kind(kind, &[PaneId]) -> LayoutNode`,
  the live-tree twin of `LayoutPlan::realize`, proven against the shipped
  tiling property for every kind × pane-count. *(Even arrangements set
  ratio `1/n` per split level, not a balanced `0.5`.)*
- **Searchable ranking seam** — lift `praca::index::best_match` to a
  `Searchable` trait both `SessionRecord` and `SessionDefinition` impl
  (`SessionDefinition` already carries `custom_name`/`name_seed`/`tags`/
  `visits`/`last_seen`; needs `identity()`/`name_word()`/`keywords()`
  delegating to the shared `identity_for` helper). `rank_union` proves a
  latent def and a live record interleave by one frecency+fuzzy order.
- **`get_window_layout`** MCP read tool (tear-MCP, read-only):
  `{panes:[{pane_id, rect}], active_pane, valid|LayoutError}` projected
  from the shipped `LayoutNode` — zero new wire.
- **Apply a named `LayoutKind` preset** to a window's existing panes
  (`from_kind` + `validate` + re-tile) — daemon-side via tear-client now;
  the mado-MCP `apply_layout_preset` write verb.
- **`save-as-preset` (single-pane)** — capture the focused live session's
  cwd+shell into a `SessionDefinition::single_pane` in an additive in-mado
  catalog. Bootstraps the `(defsession)` authoring leg without the Lisp
  parser or a disk-format change.
- **`RestoreReport`** — a plain typed value the toast renders stating
  exactly what a (future) instantiate restored (layout+shells) vs didn't
  (ratios balanced-not-saved, scrollback fresh). Tier-honesty as UX.
- **Daemon-side split/resize/navigate/close** over tear-client from the
  shell (not mado — mado can't *draw* multi-pane until M5; the model ops
  are usable now).

### Needs-M2 (the production rewire — paused)

Union picker over live *and* latent in one ranked space with
`RowKind::Instantiate`; exact `●N` count badges + def-grouped collapse;
Instantiate-on-Enter wired to live spawn + cd-auto-instantiate;
`decide()→AttachAction`; `ProjectBinding→InstanceRegistry`; `PracaSnapshot.definitions`;
durable preset frecency; reinstantiate-on-attach replaying the saved
layout; recycle-this-instance; mirror-to-window; the instantiate/list-
instances mado-MCP write verbs; multi-pane `save-as-preset` via the
`LayoutNode→LayoutPlan` dehydrate inverse (must ship with a
`dehydrate(realize(plan)) == identity` round-trip test); split-ratio
fidelity on instantiate.

### Needs-M5 (mado renders multi-pane)

mado draws the multi-pane window from `compute_rects` (re-add
`render_multi_pane`, retire the `terminal.rs` double-parse); view-local vs
model focus divergence across two windows; per-pane kitty/sixel across
viewports (the graphics seam); `(defsession)` authored presets feeding the
union; restart-durable live PTYs + scrollback (tmux-resurrect class — a
model change, not an extension).

---

## 4. The first brick (verified buildable-now)

**One commit, two pure functions, zero live state — no mado, no daemon, no
socket, no on-disk format touched:**

1. `LayoutKind::from_kind(kind, &[PaneId]) -> LayoutNode` — arranges an
   ordered slice of a window's existing pane ids into a `validate()`-passing
   `LayoutNode` using only `LayoutNode::split`/`leaf`/`Split{ratio}`. Even
   arrangements use ratio `1/n`. Proven against the shipped tiling property
   (`compute_rects` tiles bounds exactly) for every `LayoutKind` ×
   pane-count. `Custom` has no constructor (the tree is its own truth).
2. The `Searchable` trait lift — generalize `best_match`/`frec` over a
   trait; `SessionRecord` impls it from existing methods, `SessionDefinition`
   impls it via the shared `identity_for`. A `rank_union` test proves a
   latent `SessionDefinition::single_pane` and a live `SessionRecord`
   interleave by one frecency+fuzzy order.

**Why this first:** it is the single primitive every layout/preset/save
feature converges on (manual split/resize land in `Custom`; `from_kind` is
how an operator returns to a named arrangement, *and* what `instantiate`
builds from a saved plan), the `Searchable` seam unblocks all union
ranking, and both are unit-testable against shipped invariants with no
daemon running. It makes the daemon's layout authorable-as-data before any
M5 pixel work — giving the M5 renderer a typed target to draw.

---

## 5. Rejected — what NOT to build (and why)

- A `definitions` field on `PracaSnapshot` as a first brick — mutates the
  live daemon's on-disk format. **Needs-M2.**
- Exact `●N` live-instance badge pre-M2 — mado knows liveness via 1:1
  `ProjectBinding`, no def→N grouping. Only `○/●` is honest. **Needs-M2.**
- `decide()→AttachAction` so cd auto-instantiates a cold preset — *the* M2
  rewire. **Needs-M2.**
- "Reuse `best_match` verbatim" — it is private + concrete on
  `&SessionRecord`. The `Searchable` lift is real (additive) work, not free
  reuse.
- `LayoutNode→LayoutPlan` dehydrate as the first brick — a new inverse with
  no proof; must ship with a round-trip test. **Needs-M2.**
- Mirror-to-window; view-vs-model focus divergence; restart-durable
  sessions — **needs-M5** (rendering / model change).
- **Any feature as a bespoke mode/popup or a second decision enum** —
  rejected per the no-overlap law.

---

Cross-linked from [`PRACA.md`](./PRACA.md) and
[`SESSION-TYPESCAPE.md`](./SESSION-TYPESCAPE.md). Commit trail +
provenance: `memory/project_tear_session_typescape.md`.
