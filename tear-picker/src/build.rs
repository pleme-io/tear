use std::collections::HashSet;
use std::path::Path;

use serde::{Deserialize, Serialize};
use tear_types::{MultiplexerControl, PaneId, SessionId};

use crate::badge::{BadgeMode, row_label};
use crate::presets;
use crate::row::{CreateSpec, RowKind, SessionPickerRow};

pub const ROWS_DEFAULT: usize = 12;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(default)]
pub struct UnionOptions {
    pub surface_presets: bool,
    pub badges: BadgeMode,
}

impl Default for UnionOptions {
    fn default() -> Self {
        Self {
            surface_presets: true,
            badges: BadgeMode::Auto,
        }
    }
}

#[must_use]
pub fn first_pane_of(control: &dyn MultiplexerControl, session: SessionId) -> Option<PaneId> {
    control
        .get_session(session)
        .ok()
        .and_then(|s| s.windows.values().next().map(|w| w.active_pane))
}

#[must_use]
pub fn union_rows<S, U>(
    praca: &praca::Praca,
    first_pane: &dyn Fn(SessionId) -> Option<PaneId>,
    opts: UnionOptions,
    query: &str,
    now: u64,
) -> Vec<SessionPickerRow<S, U>> {
    let live_recs: Vec<&praca::SessionRecord> = praca
        .index
        .all()
        .iter()
        .filter(|rec| first_pane(rec.id).is_some())
        .collect();
    let live_roots: HashSet<&Path> = live_recs.iter().map(|r| r.project_root.as_path()).collect();
    let mut items: Vec<praca::Ranked> = live_recs.iter().map(|r| praca::Ranked::Live(*r)).collect();
    if opts.surface_presets {
        for def in praca.definitions.all() {
            if !live_roots.contains(def.project_root.as_path()) {
                items.push(praca::Ranked::Latent(def));
            }
        }
    }
    let has_live = items.iter().any(|r| matches!(r, praca::Ranked::Live(_)));
    let has_latent = items.iter().any(|r| matches!(r, praca::Ranked::Latent(_)));
    let badge_rows = opts.badges.badges(has_live, has_latent);
    let mut rows: Vec<SessionPickerRow<S, U>> = praca::rank_mixed(items, query, now)
        .into_iter()
        .map(|ranked| match ranked {
            praca::Ranked::Live(rec) => SessionPickerRow {
                label: row_label(
                    &rec.display_name(),
                    &rec.project_root,
                    badge_rows.then_some(false),
                ),
                kind: RowKind::Switch(rec.id),
                urgency: None,
            },
            praca::Ranked::Latent(def) => SessionPickerRow {
                label: row_label(
                    &def.display_name(),
                    &def.project_root,
                    badge_rows.then_some(true),
                ),
                kind: RowKind::Instantiate(def.def_id),
                urgency: None,
            },
        })
        .collect();

    if query.trim().is_empty() {
        let mut by_recency = live_recs.clone();
        by_recency.sort_by(|a, b| b.last_seen.cmp(&a.last_seen));
        if let Some(came_from) = by_recency.get(1).map(|r| r.id) {
            if let Some(pos) = rows
                .iter()
                .position(|row| matches!(row.kind, RowKind::Switch(id) if id == came_from))
            {
                let row = rows.remove(pos);
                rows.insert(0, row);
            }
        }
    }
    rows
}

pub fn insert_band<T>(rows: &mut Vec<T>, band: Vec<T>, reserved: usize, anchor: usize) {
    if band.is_empty() {
        return;
    }
    if reserved > 0 {
        let reserve = reserved.min(band.len()).min(anchor.saturating_sub(1));
        let cut = rows.len().min(anchor - reserve);
        let tail = rows.split_off(cut);
        rows.extend(band);
        rows.extend(tail);
    } else {
        rows.extend(band);
    }
}

#[must_use]
pub fn create_surface<S, U>(
    query: &str,
    style: praca::SessionNameStyle,
) -> Vec<SessionPickerRow<S, U>> {
    let found = presets::matching(query, style);
    if !found.is_empty() {
        return found
            .into_iter()
            .map(|p| SessionPickerRow {
                label: p.label,
                kind: RowKind::Create(CreateSpec::Preset {
                    pool_index: p.pool_index,
                }),
                urgency: None,
            })
            .collect();
    }
    let q = query.trim();
    if q.is_empty() {
        return Vec::new();
    }
    let mut label = String::from("\u{ff0b} create ");
    label.push_str(q);
    vec![SessionPickerRow {
        label,
        kind: RowKind::Create(CreateSpec::Named { name: q.to_owned() }),
        urgency: None,
    }]
}

#[must_use]
pub fn session_rows<S, U>(
    praca: &praca::Praca,
    first_pane: &dyn Fn(SessionId) -> Option<PaneId>,
    opts: UnionOptions,
    query: &str,
    now: u64,
    band: Vec<SessionPickerRow<S, U>>,
    reserved: usize,
) -> Vec<SessionPickerRow<S, U>> {
    let mut rows = union_rows(praca, first_pane, opts, query, now);
    let reserved = if query.trim().is_empty() { reserved } else { 0 };
    insert_band(&mut rows, band, reserved, ROWS_DEFAULT);
    if !rows.is_empty() {
        return rows;
    }
    create_surface(query, praca.name_style)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::convert::Infallible;
    use std::path::PathBuf;

    type Row = SessionPickerRow<Infallible, Infallible>;

    fn record(name: &str, root: &str, seen: u64) -> praca::SessionRecord {
        let id = SessionId::from_seed(name);
        let mut rec = praca::SessionRecord::for_project(
            id,
            PathBuf::from(root),
            ishou_tokens::SessionNameStyle::Emoji,
            seen,
        );
        rec.rename(name.to_owned());
        rec
    }

    fn alive(_: SessionId) -> Option<PaneId> {
        Some(PaneId::from_seed("p"))
    }

    fn dead(_: SessionId) -> Option<PaneId> {
        None
    }

    #[test]
    fn a_session_without_a_live_pane_is_not_listed() {
        let mut p = praca::Praca::new();
        p.index.upsert(record("alpha", "/code/alpha", 10));
        let rows: Vec<Row> = union_rows(&p, &dead, UnionOptions::default(), "", 100);
        assert!(rows.is_empty());
    }

    #[test]
    fn live_sessions_become_switch_rows() {
        let mut p = praca::Praca::new();
        p.index.upsert(record("alpha", "/code/alpha", 10));
        let rows: Vec<Row> = union_rows(&p, &alive, UnionOptions::default(), "", 100);
        assert_eq!(rows.len(), 1);
        assert!(matches!(rows[0].kind, RowKind::Switch(_)));
        assert!(rows[0].label.contains("alpha"));
        assert!(!rows[0].label.starts_with('\u{25cf}'));
    }

    #[test]
    fn always_badges_even_an_all_live_list() {
        let mut p = praca::Praca::new();
        p.index.upsert(record("alpha", "/code/alpha", 10));
        let opts = UnionOptions {
            badges: BadgeMode::Always,
            ..UnionOptions::default()
        };
        let rows: Vec<Row> = union_rows(&p, &alive, opts, "", 100);
        assert!(rows[0].label.starts_with('\u{25cf}'));
    }

    #[test]
    fn the_session_you_came_from_is_hoisted_on_an_empty_query() {
        let mut p = praca::Praca::new();
        p.index.upsert(record("alpha", "/code/alpha", 10));
        p.index.upsert(record("bravo", "/code/bravo", 20));
        p.index.upsert(record("charlie", "/code/charlie", 30));
        let rows: Vec<Row> = union_rows(&p, &alive, UnionOptions::default(), "", 100);
        let came_from = SessionId::from_seed("bravo");
        assert!(matches!(rows[0].kind, RowKind::Switch(id) if id == came_from));
    }

    #[test]
    fn an_unmatched_query_offers_to_create_it() {
        let p = praca::Praca::new();
        let rows: Vec<Row> = session_rows(
            &p,
            &alive,
            UnionOptions::default(),
            "zzzqqq-nomatch",
            100,
            Vec::new(),
            0,
        );
        assert_eq!(rows.len(), 1);
        assert_eq!(
            rows[0].kind,
            RowKind::Create(CreateSpec::Named {
                name: "zzzqqq-nomatch".into()
            })
        );
    }

    #[test]
    fn an_empty_union_on_an_empty_query_offers_every_preset() {
        let p = praca::Praca::new();
        let rows: Vec<Row> =
            session_rows(&p, &alive, UnionOptions::default(), "", 100, Vec::new(), 0);
        assert!(!rows.is_empty());
        assert!(
            rows.iter()
                .all(|r| matches!(r.kind, RowKind::Create(CreateSpec::Preset { .. })))
        );
    }

    #[test]
    fn the_reserved_band_lands_inside_the_window() {
        let mut rows: Vec<u32> = (0..20).collect();
        insert_band(&mut rows, vec![100, 101], 2, ROWS_DEFAULT);
        assert_eq!(rows.iter().position(|r| *r == 100), Some(ROWS_DEFAULT - 2));
        assert_eq!(rows.len(), 22);
    }

    #[test]
    fn without_a_reservation_the_band_is_appended() {
        let mut rows: Vec<u32> = (0..20).collect();
        insert_band(&mut rows, vec![100], 0, ROWS_DEFAULT);
        assert_eq!(rows.last(), Some(&100));
    }

    #[test]
    fn a_nonempty_query_never_reserves() {
        let mut p = praca::Praca::new();
        for n in 0..12 {
            p.index.upsert(record(
                &format!("sess{n}"),
                &format!("/code/sess{n}"),
                10 + n,
            ));
        }
        let band: Vec<Row> = vec![SessionPickerRow {
            label: "band".into(),
            kind: RowKind::Create(CreateSpec::Named { name: "b".into() }),
            urgency: None,
        }];
        let rows = session_rows(&p, &alive, UnionOptions::default(), "sess", 100, band, 3);
        assert_eq!(rows.len(), 13);
        assert_eq!(rows.last().map(|r| r.label.as_str()), Some("band"));
    }
}
