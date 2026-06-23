# The Session Typescape — the unified session/layout/multiplexer model

> **Status: tier-honest.** This doc leads with a `Shipped / M2 / M5 /
> Phase-6` ledger and grades every claim. Per the pleme-io
> UNREPRESENTABILITY discipline: a `Result::Err` is *mitigation*; a
> compile error / absent value is *unrepresentability*. We never round a
> runtime check up to a guarantee. Where a guarantee is only-mitigated, we
> say so and name the destination — or name it a permanent **ceiling**
> (best-possible) and say why.

This is the typed model behind tear's sessions, windows, panes, layouts,
and the cd-driven attach engine — the thing [`PRACA.md`](./PRACA.md)
orchestrates. The user-facing capabilities derived from this model — the
unified Ctrl-S surface, the 3-wave extension, the buildable-now/M2/M5
partition — are in [`SESSION-FEATURESET.md`](./SESSION-FEATURESET.md). It exists because the *prose* version of "running multiple
mado instances, different sessions, session presets, instantiated and
not, split-screens, persistence" over-claimed: an 8-lens adversarial
pressure-test found ten illegal states the code actually admitted. The
verdict was **"over-claimed, not wrong-headed"** — the *shape* was right,
the *tiers* were dishonest. This doc is the corrected model: the same
shape, with each illegal state made unrepresentable by construction (or
honestly graded otherwise).

---

## 1. The two axes

A session lives at the intersection of two orthogonal axes. The pressure-
test's core finding was that the prose collapsed both.

- **Axis 1 — Definition ↔ Instance.** A *definition* is a durable,
  project-stable shape (what to run, in what layout). An *instance* is an
  ephemeral, spawn-unique running incarnation. One definition → **N**
  concurrent instances. These are **two distinct identities**
  (`DefinitionId` vs `InstanceId`), not one.

- **Axis 2 — Model ↔ View.** The session *model* (windows, panes, the
  live layout tree, focus) lives **once**, in the tear-daemon. A *view*
  (a mado window) is a read projection: it chooses which pane it displays
  and at what geometry, but it owns no session state.

**Layout is intrinsic to the model, not a separate concern.** A window IS
a `LayoutNode` (a binary tree of panes); split-screen is that tree
rendered by `compute_rects`. There is one layout algorithm; mado draws
from it, the daemon sizes PTYs from it.

---

## 2. The tier ledger

Every capability, graded by where it actually is. **Shipped** = in
`main`, compiling + tested. **M2** = the production rewire (designed, not
yet wired). **M5** = the mado/tear no-overlap endpoint. **Phase-6** =
restart-durable live state (a tmux-resurrect-class feature).

| Capability | Tier | Notes |
|---|---|---|
| Layout algebra (`split_leaf`/`remove_leaf`/`resize_leaf`/`compute_rects`/`neighbor`/`validate`) | **Shipped** | `tear-types::layout`; tear-core consumes it; adversarially audited |
| `LayoutNode → rect` renderer (`compute_rects`) | **Shipped** | proven to tile bounds exactly; reachable via `get_window` (no new wire) |
| Two-tier identity (`DefinitionId` / `InstanceId`) | **Shipped** | distinct types; `DefinitionId.0 == name_seed` (lossless migration) |
| Latent definition carrying a layout plan (`SessionDefinition` + `LayoutPlan`) | **Shipped** | `validate → DefinitionError`; leaves are `PaneSlot`, never live `PaneId` |
| 1:N definition→instances (`InstanceRegistry`) | **Shipped** | `BTreeMap<DefinitionId, BTreeSet<InstanceId>>` |
| `instantiate` / `reinstantiate` morphism | **Shipped** | tested vs real `InProcess`; tree shape + per-pane shell exact |
| `Durability::ProcessBound` (no restart-survival value) | **Shipped** | restart = re-instantiate, not resurrect |
| Typed `SessionOrigin` (project / ad-hoc / authored) | **Shipped** | the ad-hoc path is a typed arm, not a hidden branch |
| `AttachAction::Instantiate` branch | **Shipped** (type) / **M2** (wired) | `decide()` still returns the old `AttachDecision` |
| `decide() → AttachAction`; `SessionRecord` as a projected view; delete `SessionState::Templated`; `ProjectBinding → InstanceRegistry`; mado→one store | **M2** | the production rewire — changes the live daemon + persisted format |
| split-ratio fidelity in `instantiate` (plan ratios honored) | **M2** | today the backend's balanced split; needs a ratio-bearing split op or post-spawn resize |
| Per-pane `cwd`/`env`/`args` beyond the shell on instantiate | **M2** | needs a richer backend spawn op |
| mado renders multi-pane from `compute_rects` | **M5** | the renderer + wire are ready; mado's `render_multi_pane` was deleted at Phase 4 and must be re-added |
| no-overlap: one vte parser (retire mado's `terminal.rs` double-parse) | **M5** | the live double-parse is named tech debt |
| `(defsession …)` tatara-lisp authoring leg | **M5+** | the `TataraDomain` derive is not in the tear crate-tree; don't pull the heavy dep prematurely |
| restart-durable live sessions (PTYs + scrollback survive a daemon restart) | **Phase-6** | impossible without serialize-registry + respawn/reattach-PTY; only the *definition* survives today |

---

## 3. The illegal-state ledger

The ten states the pressure-test found, and what each became. **8 of 10
are truly-unrepresentable and shipped; 2 are permanent ceilings.**

| # | Was admittable | Now | Tier |
|---|---|---|---|
| 1 | one identity for latent + live | `DefinitionId` vs `InstanceId` — confusing them is `E0308` | **truly-unrep** ✓ |
| 2 | 1:1 binding can't hold N instances of one def | `InstanceRegistry` value is a `BTreeSet` — N is the only shape | **truly-unrep** ✓ |
| 3 | a definition carried no layout/panes/commands | `SessionDefinition` holds a **non-Option** `LayoutPlan` + a `SpawnSpec` per slot | **truly-unrep** ✓ |
| 4 | `SessionState::Templated` empty arm | deleted; the latent thing IS a `SessionDefinition` value | **truly-unrep** ✓ (type shipped; arm deleted at M2) |
| 5 | no way to instantiate a definition | `instantiate` morphism + `AttachAction::Instantiate` | **truly-unrep** ✓ |
| 6 | "live PTYs survive a daemon restart" | `Durability` has no survival arm; `reinstantiate` is the constructive "restart" | **truly-unrep** ✓ |
| 9 | ad-hoc = a silent third construction path | `SessionOrigin {Project, Adhoc, Authored}` non-Option field | **truly-unrep** ✓ |
| 10 | model focus vs view focus conflated | `TearWindow.active_pane` (shared model) vs mado's displayed-pane (view-local) — different types/crates | **truly-unrep** (mado-side, M2/M5) |
| 7 | `decide()` can return `SwitchTo(dead_id)` | resolve the instance against the live registry before switching | **only-mitigated — C2 ceiling** |
| 8 | mado builds a second unsynced store | route mado through the one daemon `PracaStore` | **only-mitigated — C4 ceiling** |

### Why #7 and #8 are *ceilings*, not debt

- **#7 (C2 — external-world observation).** Liveness is a kernel-process
  fact. A PTY can die in the window between "resolve the instance" and
  "switch to it." No type can prevent that race; the correct answer is to
  detect-and-recover at runtime (re-instantiate on `BoundSessionGone`).
  Chasing a compile error past this is wasted effort.
- **#8 (C4 — irreducibly-runtime path).** The canonical store is reachable
  by a runtime path (a socket / file). Deleting the second store removes
  the bug; making "a second store cannot be constructed" a *compile* error
  would require sealing a runtime resource, which the OS does not offer.

These two are best-possible at the runtime tier. Everything else is a
compile error.

---

## 4. The type model

Crate homes obey no-overlap: `tear-types` owns the shared model,
`tear-core` owns runtime/PTYs, `praca` owns orchestration/definitions,
`mado` owns the view. Dependency direction is one-way: `praca → tear-types`;
`tear-core` does **not** depend on `praca`.

### Borders (`tear-types`)

```
DefinitionId   durable, project-stable; inner == stable_seed(project_root)
InstanceId     = SessionId (alias); spawn-unique daemon handle
PaneSlot       definition-local pane key (never a live PaneId)
SpawnSpec      what to spawn in a slot (shell/args/env/cwd/title/policy)
LayoutPlan     latent layout tree over PaneSlots; ::realize(mint) → LayoutNode
WindowPlan     latent TearWindow (name + LayoutPlan + active_slot)
Durability     { ProcessBound }   — no restart-surviving value exists
LiveSession    TearSession + the typed live→DefinitionId link + Durability
LayoutNode     the live layout tree + the shipped algebra (the renderer)
Rect           gap-free/overlap-free cell rectangles
```

### Orchestration (`praca`)

```
SessionDefinition   the durable shape: id + origin + naming + project + plan
SessionOrigin       { Project, Adhoc{theme,seed}, Authored }
InstanceRegistry    BTreeMap<DefinitionId, BTreeSet<InstanceId>>  (1:N)
AttachAction        { Stay, SwitchTo(InstanceId), Instantiate(DefinitionId), SpawnNew{..} }
```

### Morphisms

```
DefinitionId::from_project(root)              → durable id (= name_seed)
LayoutPlan::realize(&mut mint)                → LayoutNode (slot leaves → minted PaneIds)
instantiate(def, &dyn MultiplexerControl)     → LiveSession   (triplet leg 3)
reinstantiate(def, backend)                   → LiveSession   (constructive "restart")
SessionRecord ⇄ SessionDefinition             projection (M2 migration seam)
```

The interpreter takes `&dyn MultiplexerControl` — the **shipped,
object-safe trait** — so a test double or the real `tear-core::InProcess`
are interchangeable (the Environment-trait testability contract, for
free, with no new trait and no dependency cycle).

---

## 5. The (defsession) triplet

The latent definition is one instance of the pleme-io TYPED-SPEC +
INTERPRETER TRIPLET:

1. **Typed border** — the `tear-types` plan types + `SessionDefinition`
   (shipped, plain serde).
2. **Authored Lisp** — `(defsession …)` (M5+; deferred because the
   `TataraDomain` derive is not in the tear crate-tree and pulling it is a
   heavy edge that must be justified).
3. **Working interpreter** — `instantiate` (shipped), driving the
   `MultiplexerControl` Environment (mockable by construction).

---

## 6. How this answers the original questions

- **Running multiple mado instances / sessions** — N `InstanceId`s of one
  `DefinitionId`; the registry is 1:N by type.
- **Session presets, instantiated and not** — a preset is a
  `SessionDefinition` (latent); a running one is a `LiveSession`. They are
  one navigated space, distinguished by which axis-1 side they sit on.
- **Split-screens and layouts** — a window IS a `LayoutNode`; split-screen
  is `compute_rects` rendered by a view. One algorithm, both apps.
- **Persistence across all this** — the *definition* is durable (survives
  restart); the *instance* is `ProcessBound` (survives client disconnect,
  not daemon restart). "Restart the session" = `reinstantiate` the
  definition. Restart-durable live PTYs are a named Phase-6 feature, not a
  silent promise.

---

## 7. The destination + plan

- **M2 (production rewire).** `decide() → AttachAction`; `SessionRecord`
  becomes a projected view of a `SessionDefinition` + registry entry;
  delete `SessionState::Templated`; migrate `ProjectBinding →
  InstanceRegistry`; route mado through the one `PracaStore`. Behavior-
  changing + a persisted-format migration — staged with backward-compat
  deserialization.
- **M5 (no-overlap endpoint).** mado renders multi-pane from
  `compute_rects`; retire mado's `terminal.rs` double-parse so the session
  model lives once. Resolve the graphics seam (kitty/sixel placement state
  the cell snapshot can't carry): either grow the snapshot to own graphics
  or restate no-overlap as "text non-overlapping; graphics intentionally
  mado-local."
- **Phase-6.** Restart-durable live sessions: serialize the registry +
  grids, respawn/reattach PTYs. Only attempt once the model above is
  load-bearing.

Provenance: model chosen via a 4-framing design panel → adversarial
synthesis (backbone = the triplet/crate-seam framing). The algebra was
adversarially audited (6 skeptics, 3 real bugs fixed). See
`memory/project_tear_session_typescape.md` for the commit trail.
