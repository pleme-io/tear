//! [`SessionIndex`] — the searchable, ranked catalog of every session
//! praca knows about.
//!
//! Backs two operator surfaces:
//!
//! * the **picker** (the fallback when automation doesn't fire): an
//!   empty query returns every session ranked by frecency; a non-empty
//!   query fuzzy-filters then ranks by `(match quality, frecency)`.
//! * **by-project lookup**: the attach engine asks "is there already a
//!   session for this root?" via [`SessionIndex::by_project`].
//!
//! Search matches a small subsequence-fuzzy scorer against the session's
//! name *word* (`tide`/`frost`/…), its `cwd`, and its `tags`. No heavy
//! fuzzy dep — the population is session-scale (tens), so a simple
//! contiguity-rewarding subsequence scorer is plenty and keeps the crate
//! dependency-light.
//!
//! All time is `u64` unix-seconds INJECTED into [`SessionIndex::search`]
//! — the index never reads the clock.

use serde::{Deserialize, Serialize};
use tear_types::id::SessionId;

use crate::frecency;
use crate::record::SessionRecord;

/// In-memory catalog of [`SessionRecord`]s, keyed by [`SessionId`].
///
/// Serialises **transparently** as the underlying `Vec<SessionRecord>`,
/// so the persisted form is a plain JSON array of records — the daemon's
/// praça store (M1) round-trips an index byte-for-byte across restarts.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct SessionIndex {
    records: Vec<SessionRecord>,
}

impl SessionIndex {
    /// An empty index.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Insert `record`, or replace the existing record with the same
    /// [`SessionId`]. Returns the displaced record, if any.
    pub fn upsert(&mut self, record: SessionRecord) -> Option<SessionRecord> {
        if let Some(slot) = self.records.iter_mut().find(|r| r.id == record.id) {
            Some(std::mem::replace(slot, record))
        } else {
            self.records.push(record);
            None
        }
    }

    /// Remove the record with `id`. Returns it if present.
    pub fn remove(&mut self, id: SessionId) -> Option<SessionRecord> {
        if let Some(pos) = self.records.iter().position(|r| r.id == id) {
            Some(self.records.remove(pos))
        } else {
            None
        }
    }

    /// Borrow the record with `id`.
    #[must_use]
    pub fn get(&self, id: SessionId) -> Option<&SessionRecord> {
        self.records.iter().find(|r| r.id == id)
    }

    /// Mutably borrow the record with `id`.
    pub fn get_mut(&mut self, id: SessionId) -> Option<&mut SessionRecord> {
        self.records.iter_mut().find(|r| r.id == id)
    }

    /// The session bound to a given project `root`, if one is tracked.
    /// First match wins (a project root maps to at most one live
    /// session in normal operation).
    #[must_use]
    pub fn by_project(&self, root: &std::path::Path) -> Option<&SessionRecord> {
        self.records.iter().find(|r| r.project_root == root)
    }

    /// Number of tracked sessions.
    #[must_use]
    pub fn len(&self) -> usize {
        self.records.len()
    }

    /// Whether the index is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.records.is_empty()
    }

    /// All records, unranked, in insertion order.
    #[must_use]
    pub fn all(&self) -> &[SessionRecord] {
        &self.records
    }

    /// Search the index.
    ///
    /// * An **empty / whitespace-only** `query` returns EVERY record
    ///   ranked by frecency descending (the picker's default view).
    /// * A non-empty `query` keeps only records whose name word, cwd, or
    ///   any tag fuzzy-matches, ranked by `(best match quality desc,
    ///   frecency desc)`.
    ///
    /// `now` is unix-seconds, injected for the frecency tie-break.
    #[must_use]
    pub fn search(&self, query: &str, now: u64) -> Vec<&SessionRecord> {
        let q = query.trim();
        if q.is_empty() {
            let mut out: Vec<&SessionRecord> = self.records.iter().collect();
            out.sort_by(|a, b| {
                frec(b, now)
                    .partial_cmp(&frec(a, now))
                    .unwrap_or(std::cmp::Ordering::Equal)
                    // deterministic final tie-break on id
                    .then(a.id.0.cmp(&b.id.0))
            });
            return out;
        }

        let mut scored: Vec<((i32, i32), f64, &SessionRecord)> = self
            .records
            .iter()
            .filter_map(|r| best_match(q, r).map(|m| (m, frec(r, now), r)))
            .collect();

        scored.sort_by(|a, b| {
            // `(field_tier, fuzzy_quality)` first — the tuple compares
            // lexicographically, so a name match outranks a tag match
            // outranks a path match regardless of raw fuzzy quality, and
            // ties within a tier fall to quality. Then frecency, then id.
            b.0.cmp(&a.0)
                .then(b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal))
                .then(a.2.id.0.cmp(&b.2.id.0))
        });

        scored.into_iter().map(|(_, _, r)| r).collect()
    }
}

/// Frecency of a record at `now`.
fn frec(r: &SessionRecord, now: u64) -> f64 {
    frecency::score(r.visits, r.last_seen, now)
}

/// Field tiers — a match on the session **name** ranks above a match on a
/// **tag**, which ranks above a match on the **cwd/path**, regardless of
/// raw fuzzy quality. This is the operator's rule: *"a name match should
/// be a higher tier in the session search."* The `(tier, quality)` pair
/// compares lexicographically, so the tier dominates and quality breaks
/// within-tier ties.
const TIER_NAME: i32 = 3;
const TIER_TAG: i32 = 2;
const TIER_PATH: i32 = 1;

/// Best `(field_tier, fuzzy_quality)` of `query` against any searchable
/// field of `record` (name word, tags, cwd string). `None` if nothing
/// matches. The tier dominates ranking (see [`TIER_NAME`] et al.).
fn best_match(query: &str, record: &SessionRecord) -> Option<(i32, i32)> {
    let cwd = record.cwd.to_string_lossy();
    let mut best: Option<(i32, i32)> = None;
    let mut consider = |tier: i32, hay: &str| {
        if let Some(q) = fuzzy_score(query, hay) {
            let cand = (tier, q);
            best = Some(best.map_or(cand, |b| b.max(cand)));
        }
    };
    consider(TIER_NAME, record.name_word());
    for t in &record.tags {
        consider(TIER_TAG, t);
    }
    consider(TIER_PATH, &cwd);
    best
}

/// Case-insensitive subsequence fuzzy scorer.
///
/// Returns `Some(score)` if every char of `needle` appears in `haystack`
/// in order, else `None`. Higher is better. Rewards:
/// * contiguous runs of matched chars (`+contiguous_len`),
/// * a match at the haystack start (`+bonus`),
/// * matches right after a separator (`/`, `-`, `_`, `.`, space) — word
///   boundaries (`+bonus`),
/// and lightly penalizes a longer haystack so a tight match on a short
/// field outranks the same subsequence buried in a long path.
///
/// An empty needle scores 0 against any haystack (matches everything) —
/// callers route the empty-query case to frecency-only before reaching
/// here.
#[must_use]
pub fn fuzzy_score(needle: &str, haystack: &str) -> Option<i32> {
    let needle = needle.to_lowercase();
    let haystack_lc = haystack.to_lowercase();
    let n: Vec<char> = needle.chars().collect();
    if n.is_empty() {
        return Some(0);
    }
    let hay: Vec<char> = haystack_lc.chars().collect();

    let is_sep = |c: char| matches!(c, '/' | '-' | '_' | '.' | ' ');

    let mut ni = 0usize;
    let mut score = 0i32;
    let mut run = 0i32;
    for (hi, &hc) in hay.iter().enumerate() {
        if ni < n.len() && hc == n[ni] {
            run += 1;
            score += run; // contiguity reward grows within a run
            if hi == 0 {
                score += 8; // start-of-string bonus
            } else if is_sep(hay[hi - 1]) {
                score += 6; // word-boundary bonus
            }
            ni += 1;
        } else {
            run = 0;
        }
    }
    if ni == n.len() {
        // light length penalty: tighter haystack wins ties.
        Some(score - (hay.len() as i32 / 16))
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ishou_tokens::SessionNameStyle;
    use std::path::{Path, PathBuf};

    const NOW: u64 = 1_000_000_000;

    fn rec(id: &str, root: &str, visits: u32, last_seen: u64, tags: &[&str]) -> SessionRecord {
        let mut r = SessionRecord::for_project(
            SessionId::from_seed(id),
            PathBuf::from(root),
            SessionNameStyle::Emoji,
            last_seen,
        );
        r.visits = visits;
        r.tags = tags.iter().map(|t| (*t).to_string()).collect();
        r
    }

    #[test]
    fn upsert_replaces_same_id() {
        let mut idx = SessionIndex::new();
        let a = rec("s", "/code/mado", 1, NOW, &[]);
        assert!(idx.upsert(a).is_none());
        let mut b = rec("s", "/code/mado", 5, NOW, &[]);
        b.visits = 5;
        let old = idx.upsert(b).unwrap();
        assert_eq!(old.visits, 1);
        assert_eq!(idx.len(), 1);
        assert_eq!(idx.get(SessionId::from_seed("s")).unwrap().visits, 5);
    }

    #[test]
    fn remove_and_get() {
        let mut idx = SessionIndex::new();
        idx.upsert(rec("s", "/x", 1, NOW, &[]));
        assert!(idx.get(SessionId::from_seed("s")).is_some());
        assert!(idx.remove(SessionId::from_seed("s")).is_some());
        assert!(idx.get(SessionId::from_seed("s")).is_none());
        assert!(idx.remove(SessionId::from_seed("s")).is_none());
    }

    #[test]
    fn by_project_finds_root() {
        let mut idx = SessionIndex::new();
        idx.upsert(rec("a", "/code/mado", 1, NOW, &[]));
        idx.upsert(rec("b", "/code/tear", 1, NOW, &[]));
        assert_eq!(
            idx.by_project(Path::new("/code/tear")).unwrap().id,
            SessionId::from_seed("b")
        );
        assert!(idx.by_project(Path::new("/code/nope")).is_none());
    }

    #[test]
    fn empty_query_returns_all_by_frecency() {
        let mut idx = SessionIndex::new();
        idx.upsert(rec("stale", "/code/a", 20, NOW - 14 * 24 * 3600, &[])); // 20 * 0.25 = 5
        idx.upsert(rec("fresh", "/code/b", 2, NOW - 60, &[])); // 2 * 4 = 8
        let out = idx.search("", NOW);
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].id, SessionId::from_seed("fresh"));
        assert_eq!(out[1].id, SessionId::from_seed("stale"));
    }

    #[test]
    fn name_match_is_a_higher_tier_than_tag_match() {
        // The operator's rule: a session whose NAME matches the query
        // must outrank a session that only matches via a TAG, regardless
        // of fuzzy quality or frecency.
        let r = rec("x", "/code/qqq", 1, NOW, &["xkcdtag"]);
        let name = r.name_word(); // the emoji's searchable word
        let name_tier = best_match(name, &r).expect("name matches").0;
        let tag_tier = best_match("xkcdtag", &r).expect("tag matches").0;
        assert_eq!(name_tier, TIER_NAME);
        assert_eq!(tag_tier, TIER_TAG);
        assert!(name_tier > tag_tier, "a name match must outrank a tag match");
    }

    #[test]
    fn query_filters_to_matching_records() {
        let mut idx = SessionIndex::new();
        idx.upsert(rec("a", "/code/pleme-io/mado", 1, NOW, &[]));
        idx.upsert(rec("b", "/code/pleme-io/tear", 1, NOW, &[]));
        let out = idx.search("mado", NOW);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].id, SessionId::from_seed("a"));
    }

    #[test]
    fn query_matches_tags() {
        let mut idx = SessionIndex::new();
        idx.upsert(rec("a", "/code/x", 1, NOW, &["infra", "deploy"]));
        idx.upsert(rec("b", "/code/y", 1, NOW, &["frontend"]));
        let out = idx.search("deploy", NOW);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].id, SessionId::from_seed("a"));
    }

    #[test]
    fn match_quality_outranks_frecency() {
        // `b` is much fresher/more-visited, but `a`'s cwd is a tighter
        // match for the query. Match quality wins over frecency.
        let mut idx = SessionIndex::new();
        idx.upsert(rec("a", "/deploy", 1, NOW - 3 * 24 * 3600, &[])); // tight cwd match, low frecency
        idx.upsert(rec("b", "/x/y/z/deeply/buried/deploy/path", 50, NOW, &[])); // buried match, high frecency
        let out = idx.search("deploy", NOW);
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].id, SessionId::from_seed("a"), "tighter match ranks first");
    }

    #[test]
    fn fuzzy_subsequence_matches_non_contiguous() {
        // "dpl" is a subsequence of "deploy".
        assert!(fuzzy_score("dpl", "deploy").is_some());
        // "xyz" is not.
        assert!(fuzzy_score("xyz", "deploy").is_none());
    }

    #[test]
    fn fuzzy_is_case_insensitive() {
        assert!(fuzzy_score("MADO", "code/mado").is_some());
    }

    #[test]
    fn frecency_tie_break_within_equal_match() {
        // Two equal substring matches on cwd; the fresher one ranks first.
        let mut idx = SessionIndex::new();
        idx.upsert(rec("old", "/work/api", 1, NOW - 5 * 24 * 3600, &[]));
        idx.upsert(rec("new", "/work/api", 1, NOW - 60, &[]));
        let out = idx.search("api", NOW);
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].id, SessionId::from_seed("new"));
    }
}
