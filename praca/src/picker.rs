//! The union picker projection — the pure praça function that turns the
//! live-session index + the latent-preset catalog + the instance registry
//! into the ranked list of rows `Ctrl-S` shows.
//!
//! This encapsulates the picker's *decision* logic — which verb each row
//! dispatches — in praça (orchestration), so mado stays a pure view that
//! just draws labels + badges. Per the no-overlap law, "which
//! [`AttachAction`] does Enter perform on this row" lives here, never in
//! mado's renderer. mado consumes [`Vec<UnionRow>`] and draws plain text +
//! a badge glyph; it never holds a praça type.
//!
//! The projection realizes the design's disjoint union:
//! `{live instances → Switch} ∪ {latent presets with no live instance →
//! Instantiate}`, ranked by the one shared comparator
//! ([`crate::index::rank_mixed`]).

use crate::attach::AttachAction;
use crate::definition::SessionDefinition;
use crate::index::{rank_mixed, Ranked};
use crate::instance_registry::InstanceRegistry;
use crate::record::SessionRecord;

/// One row of the union picker: a plain label, a state badge, and the verb
/// Enter performs. `label` is plain text — no praça type leaks into mado's
/// renderer.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UnionRow {
    /// The display label (`🌊 tide`), already resolved — mado draws it verbatim.
    pub label: String,
    /// `true` = a running session (● live); `false` = a latent preset (○).
    pub live: bool,
    /// What Enter performs: [`AttachAction::SwitchTo`] for a live row,
    /// [`AttachAction::Instantiate`] for a preset.
    pub action: AttachAction,
}

/// Project the live index + latent catalog + registry into the ranked
/// `Ctrl-S` row list. Live sessions become Switch rows; presets with NO
/// live instance become Instantiate rows (a preset that IS running appears
/// as its live session, not a duplicate latent row). Everything ranked by
/// the one shared order (empty query → frecency; non-empty → fuzzy).
#[must_use]
pub fn union_view(
    records: &[SessionRecord],
    defs: &[SessionDefinition],
    registry: &InstanceRegistry,
    query: &str,
    now: u64,
) -> Vec<UnionRow> {
    let mut items: Vec<Ranked> = records.iter().map(Ranked::Live).collect();
    for d in defs {
        // A preset with a live instance is represented by that instance (a
        // Switch row), never duplicated as a latent row.
        if registry.instance_count(d.def_id) == 0 {
            items.push(Ranked::Latent(d));
        }
    }
    rank_mixed(items, query, now)
        .into_iter()
        .map(|r| match r {
            Ranked::Live(rec) => UnionRow {
                label: rec.display_name(),
                live: true,
                // A record's id IS its live InstanceId.
                action: AttachAction::SwitchTo(rec.id),
            },
            Ranked::Latent(d) => UnionRow {
                label: d.display_name(),
                live: false,
                action: AttachAction::Instantiate(d.def_id),
            },
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::record::NameStyle;
    use ishou_tokens::SessionNameStyle;
    use std::path::PathBuf;
    use tear_types::SessionId;

    fn rec(id: &str, root: &str, visits: u32, last_seen: u64) -> SessionRecord {
        let mut r = SessionRecord::for_project(
            SessionId::from_seed(id),
            PathBuf::from(root),
            SessionNameStyle::Emoji,
            last_seen,
        );
        r.visits = visits;
        r
    }

    fn preset(root: &str, last_seen: u64) -> SessionDefinition {
        SessionDefinition::single_pane(root, "/bin/zsh", NameStyle::Emoji, last_seen)
    }

    #[test]
    fn live_and_latent_get_the_right_badges_and_verbs() {
        let live = rec("live", "/code/running", 1, 100);
        let p = preset("/code/preset", 100);
        let registry = InstanceRegistry::new(); // the preset is NOT running
        let rows = union_view(std::slice::from_ref(&live), std::slice::from_ref(&p), &registry, "", 100);
        assert_eq!(rows.len(), 2);
        // The running session is a ● Switch; the preset a ○ Instantiate.
        let live_row = rows.iter().find(|r| r.live).unwrap();
        assert!(matches!(live_row.action, AttachAction::SwitchTo(_)));
        let latent_row = rows.iter().find(|r| !r.live).unwrap();
        assert_eq!(latent_row.action, AttachAction::Instantiate(p.def_id));
    }

    #[test]
    fn a_preset_with_a_live_instance_is_not_a_duplicate_latent_row() {
        let p = preset("/code/x", 100);
        let mut registry = InstanceRegistry::new();
        registry.register(p.def_id, SessionId(99)); // now running
        let rows = union_view(&[], std::slice::from_ref(&p), &registry, "", 100);
        // It's represented by its live instance, not as a latent row.
        assert!(rows.is_empty());
    }

    #[test]
    fn rows_are_ranked_by_one_order_live_and_latent_interleaved() {
        // A low-frecency live session and a high-frecency latent preset:
        // empty query ranks by frecency, so the preset sorts first.
        let live = rec("l", "/code/live", 1, 100);
        let mut p = preset("/code/preset", 100);
        p.visits = 50;
        let registry = InstanceRegistry::new();
        let rows = union_view(std::slice::from_ref(&live), std::slice::from_ref(&p), &registry, "", 100);
        assert_eq!(rows.len(), 2);
        assert!(!rows[0].live, "high-frecency latent preset ranks first");
        assert!(rows[1].live);
    }
}
