# tear-picker

The renderer-agnostic session-picker substrate. Two consumers: mado's Ctrl-S overlay (GPU) and `tear attach` (crossterm). Neither owns the model.

## What lives here

| Module | Holds | Moved from mado |
|---|---|---|
| `state` | `FuzzyPicker<Row>`, `PickerSource` | `src/picker/state.rs`, verbatim, tests kept |
| `reconcile` | `IndexReconciler`, `ControlSessionReconciler` (live vs index, blind read prunes nothing) | `src/picker/reconcile.rs`, verbatim, tests kept |
| `presets` | emoji preset model (`matching`, `EmojiPreset`) | `src/picker/presets.rs`, verbatim, tests kept |
| `row` | `CreateSpec`, `RowKind<S>`, `SessionPickerRow<S, U>`, `SessionPickerState` | `src/session_picker.rs` |
| `badge` | `BadgeMode`, `row_label`, glyphs | `src/config.rs` (`BadgeMode`), `PracaPickerBridge::row_label` |
| `build` | `UnionOptions`, `union_rows`, `session_rows`, `insert_band`, `create_surface`, `first_pane_of` | body of `PracaPickerBridge::list` |
| `action` | `PickerKey`, `PickerKeyClass`, `PickerAction`, `RenameAction`, `picker_action`, `rename_action` | the Session-picker and SessionRename arms of `ux/modes.rs` |
| `testing` (feature `testing`) | `Unreachable` backend | `src/control_stub.rs` |

Kept out: GPU drawing, motion, pointer pixels, egaku, the suggestion store, `Counted` perf wrapper, the switch channel.

## Generalisation

`RowKind<S>` and `SessionPickerRow<S, U>` default `S` and `U` to `Infallible`. A consumer without suggestions never builds a `Suggestion` row. mado sets `S = SuggestionId`, `U = Urgency`.

`union_rows` and `session_rows` take a `first_pane: &dyn Fn(SessionId) -> Option<PaneId>` closure so mado can pass its `Counted` control wrapper. `first_pane_of(&dyn MultiplexerControl, id)` is the plain form.

`session_rows` takes the already built suggestion `band` and the `reserved` count. It forces `reserved` to 0 for a non-empty query.

## Config

| Type | Fields | Default |
|---|---|---|
| `UnionOptions` | `surface_presets`, `badges` | `true`, `auto` |
| `BadgeMode` | `off` / `auto` / `always` | `auto` |

Feature `schema` derives `schemars::JsonSchema` (schemars 1.2).

## Migration note for mado (phase 3, not done here)

| mado call site | Swap to |
|---|---|
| `src/picker/state.rs` | `pub use tear_picker::state::*` then delete the file |
| `src/picker/reconcile.rs` | `tear_picker::reconcile::*`; tests use `tear_picker::testing::Unreachable` |
| `src/picker/presets.rs` | `tear_picker::presets` |
| `src/control_stub.rs` | `tear_picker::testing::Unreachable` (feature `testing` in dev-dependencies) |
| `session_picker.rs` `CreateSpec`, `RowKind`, `SessionPickerRow`, `SessionPickerState` | `type SessionPickerRow = tear_picker::SessionPickerRow<SuggestionId, Urgency>` and the same for the others |
| `config.rs` `BadgeMode` | `pub use tear_picker::BadgeMode` (enable feature `schema`) |
| `PracaPickerBridge::row_label` | `tear_picker::row_label` |
| `PracaPickerBridge::first_pane_of` | `tear_picker::first_pane_of` or the closure form |
| `PracaPickerBridge::list`, live and latent part | `tear_picker::union_rows` |
| `PracaPickerBridge::list`, band insert and create fallback | `tear_picker::session_rows` (pass `suggestion_rows(...)` as `band`) |
| `crate::row_budget::ROWS_DEFAULT` anchor in `list` | `tear_picker::ROWS_DEFAULT` (12, equal to mado's) |
| `ux/modes.rs` Session picker arm | map `OverlayKeyClass` to `PickerKeyClass`, call `picker_action`, map `PickerAction` to `OverlayEffect` |
| `ux/modes.rs` SessionRename arm | same with `rename_action` |

mado's existing picker tests are the oracle: the moved tests keep their names.

## Terminal client

`tear attach` renders this model with crossterm. See `docs/ATTACH.md`.
