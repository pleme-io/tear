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
        // Delegates to the shared `rank` over `Searchable` — the comparator
        // (empty→frecency; non-empty→tier/quality/frecency/id) lives once,
        // reused verbatim by `DefinitionIndex`.
        rank(&self.records, query, now)
    }
}

/// The fields the picker's ranking reads off a candidate. Both a live
/// [`SessionRecord`] and a latent [`crate::SessionDefinition`] implement
/// it, so the ONE scorer ([`best_match`] + [`frec`]) ranks them in a
/// single frecency+fuzzy order — the additive seam the union picker
/// (`Ctrl-S` over live ∪ latent) is built on. Object-safe by design
/// (no generics, no `Self` returns) so a `&dyn Searchable` heterogeneous
/// list ranks uniformly.
pub trait Searchable {
    /// The operator's custom rename, if any (highest name-tier match).
    fn custom_name(&self) -> Option<&str>;
    /// The emoji identity's word (`"tide"` for `🌊 tide`).
    fn name_word(&self) -> &'static str;
    /// The emoji identity's synonyms (`"wave"`/`"water"` find `🌊 tide`).
    fn keywords(&self) -> &'static [&'static str];
    /// Operator tags.
    fn tags(&self) -> &[String];
    /// The project/cwd path, as a search haystack (lowest tier).
    fn path_str(&self) -> std::borrow::Cow<'_, str>;
    /// Frecency: total visits.
    fn visits(&self) -> u32;
    /// Frecency: unix-seconds of last touch.
    fn last_seen(&self) -> u64;
    /// The deterministic final tie-break — the candidate's stable id inner
    /// `u64` (a record's `SessionId`, a definition's `DefinitionId`), so a
    /// ranking is reproducible when frecency + match quality tie.
    fn rank_key(&self) -> u64;
}

/// Rank a homogeneous slice of searchable candidates by the picker's order:
/// empty query → frecency descending; non-empty → `(field_tier,
/// fuzzy_quality)` descending, then frecency, then [`Searchable::rank_key`].
/// This is the ONE ranking algorithm both [`SessionIndex`] (live records)
/// and [`crate::DefinitionIndex`] (latent presets) consume — extracted so
/// the comparator lives once.
#[must_use]
pub fn rank<'a, T: Searchable>(items: &'a [T], query: &str, now: u64) -> Vec<&'a T> {
    use std::cmp::Ordering::Equal;
    let q = query.trim();
    if q.is_empty() {
        let mut out: Vec<&T> = items.iter().collect();
        out.sort_by(|a, b| {
            frec(*b, now)
                .partial_cmp(&frec(*a, now))
                .unwrap_or(Equal)
                .then(a.rank_key().cmp(&b.rank_key()))
        });
        return out;
    }
    let mut scored: Vec<((i32, i32), f64, &T)> = items
        .iter()
        .filter_map(|r| best_match(q, r).map(|m| (m, frec(r, now), r)))
        .collect();
    scored.sort_by(|a, b| {
        b.0.cmp(&a.0)
            .then(b.1.partial_cmp(&a.1).unwrap_or(Equal))
            .then(a.2.rank_key().cmp(&b.2.rank_key()))
    });
    scored.into_iter().map(|(_, _, r)| r).collect()
}

impl Searchable for SessionRecord {
    fn custom_name(&self) -> Option<&str> {
        self.custom_name.as_deref()
    }
    fn name_word(&self) -> &'static str {
        SessionRecord::name_word(self)
    }
    fn keywords(&self) -> &'static [&'static str] {
        SessionRecord::keywords(self)
    }
    fn tags(&self) -> &[String] {
        &self.tags
    }
    fn path_str(&self) -> std::borrow::Cow<'_, str> {
        self.cwd.to_string_lossy()
    }
    fn visits(&self) -> u32 {
        self.visits
    }
    fn last_seen(&self) -> u64 {
        self.last_seen
    }
    fn rank_key(&self) -> u64 {
        self.id.0
    }
}

/// One row of a union ranking: a live instance or a latent preset.
pub enum Ranked<'a> {
    /// A running session.
    Live(&'a SessionRecord),
    /// A preset with no live instance (Enter would instantiate it).
    Latent(&'a crate::SessionDefinition),
}

impl Ranked<'_> {
    fn searchable(&self) -> &dyn Searchable {
        match self {
            Ranked::Live(r) => *r,
            Ranked::Latent(d) => *d,
        }
    }
    /// The deterministic final tie-break — the candidate's [`Searchable::rank_key`].
    fn tiebreak(&self) -> u64 {
        self.searchable().rank_key()
    }
}

/// Rank live records and latent definitions in ONE frecency+fuzzy order —
/// the same `(field_tier, quality, frecency, id)` rule [`SessionIndex::search`]
/// uses, lifted over the heterogeneous union. Empty query → frecency desc.
/// This is the picker's union view: "what is running" and "what could run"
/// interleaved, not artificially split.
#[must_use]
pub fn rank_union<'a>(
    records: &'a [SessionRecord],
    defs: &'a [crate::SessionDefinition],
    query: &str,
    now: u64,
) -> Vec<Ranked<'a>> {
    use std::cmp::Ordering::Equal;
    let q = query.trim();
    let all: Vec<Ranked<'a>> = records
        .iter()
        .map(Ranked::Live)
        .chain(defs.iter().map(Ranked::Latent))
        .collect();
    if q.is_empty() {
        let mut out = all;
        out.sort_by(|a, b| {
            frec(b.searchable(), now)
                .partial_cmp(&frec(a.searchable(), now))
                .unwrap_or(Equal)
                .then(a.tiebreak().cmp(&b.tiebreak()))
        });
        return out;
    }
    let mut scored: Vec<((i32, i32), f64, Ranked<'a>)> = all
        .into_iter()
        .filter_map(|it| best_match(q, it.searchable()).map(|m| (m, frec(it.searchable(), now), it)))
        .collect();
    scored.sort_by(|a, b| {
        b.0.cmp(&a.0)
            .then(b.1.partial_cmp(&a.1).unwrap_or(Equal))
            .then(a.2.tiebreak().cmp(&b.2.tiebreak()))
    });
    scored.into_iter().map(|(_, _, it)| it).collect()
}

/// Frecency of any searchable candidate at `now`.
fn frec(item: &dyn Searchable, now: u64) -> f64 {
    frecency::score(item.visits(), item.last_seen(), now)
}

/// Field tiers — a match on the session **name** ranks above a **keyword**
/// (emoji synonym) match, above a **tag** match, above a **cwd/path** match,
/// regardless of raw fuzzy quality. This is the operator's rule: *"a name
/// match should be a higher tier in the session search"* — extended with the
/// emoji-keyword tier so "type `wave`, find `🌊 tide`" ranks below an actual
/// name match but above tags. The `(tier, quality)` pair compares
/// lexicographically, so the tier dominates and quality breaks within-tier ties.
const TIER_NAME: i32 = 4;
const TIER_KEYWORD: i32 = 3;
const TIER_TAG: i32 = 2;
const TIER_PATH: i32 = 1;

/// Best `(field_tier, fuzzy_quality)` of `query` against any searchable
/// field of `item` (custom name, emoji word, emoji keywords, tags, path).
/// `None` if nothing matches. The tier dominates ranking (see
/// [`TIER_NAME`] et al.). Takes `&dyn Searchable` so the SAME scorer ranks
/// a live [`SessionRecord`] and a latent [`crate::SessionDefinition`].
#[must_use]
pub fn best_match(query: &str, item: &dyn Searchable) -> Option<(i32, i32)> {
    let path = item.path_str();
    let mut best: Option<(i32, i32)> = None;
    let mut consider = |tier: i32, hay: &str| {
        if let Some(q) = fuzzy_score(query, hay) {
            let cand = (tier, q);
            best = Some(best.map_or(cand, |b| b.max(cand)));
        }
    };
    // Name tier: the operator's custom rename (if any) AND the emoji word.
    if let Some(custom) = item.custom_name() {
        consider(TIER_NAME, custom);
    }
    consider(TIER_NAME, item.name_word());
    // Keyword tier: the emoji's synonyms — "wave" finds 🌊 tide.
    for kw in item.keywords() {
        consider(TIER_KEYWORD, kw);
    }
    for t in item.tags() {
        consider(TIER_TAG, t);
    }
    consider(TIER_PATH, &path);
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
    fn rank_union_interleaves_latent_and_live_by_one_frecency_order() {
        use crate::definition::SessionDefinition;
        use crate::record::NameStyle;
        // A low-frecency LIVE record and a high-frecency LATENT preset.
        // Empty query ranks by frecency — so the preset (visits 50) must
        // rank ABOVE the running session (visits 1). This proves the two
        // states interleave by ONE order, not "live always above latent".
        let live = rec("live", "/code/live-proj", 1, 100, &[]);
        let mut def = SessionDefinition::single_pane("/code/preset-proj", "/bin/zsh", NameStyle::Emoji, 100);
        def.visits = 50;
        let ranked = rank_union(std::slice::from_ref(&live), std::slice::from_ref(&def), "", 100);
        assert_eq!(ranked.len(), 2);
        assert!(matches!(ranked[0], Ranked::Latent(_)), "high-frecency preset ranks first");
        assert!(matches!(ranked[1], Ranked::Live(_)));
    }

    #[test]
    fn rank_union_fuzzy_query_ranks_both_states_through_one_scorer() {
        use crate::definition::SessionDefinition;
        use crate::record::NameStyle;
        // A live record and a latent def; a fuzzy query that matches the
        // def's project path surfaces it through the SAME best_match the
        // record uses — a def is searchable, not invisible-until-spawned.
        let live = rec("l", "/code/alpha", 1, 100, &[]);
        let def = SessionDefinition::single_pane("/code/bravo-substrate", "/bin/zsh", NameStyle::Emoji, 100);
        // "bravo" matches the def's path (TIER_PATH) but not the live one.
        let ranked = rank_union(std::slice::from_ref(&live), std::slice::from_ref(&def), "bravo", 100);
        assert_eq!(ranked.len(), 1);
        assert!(matches!(ranked[0], Ranked::Latent(_)));
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
    fn keyword_search_surfaces_the_session() {
        // The operator's example: typing an emoji synonym ("wave" → 🌊 tide)
        // finds the session via its keywords, ranked at the keyword tier.
        let mut idx = SessionIndex::new();
        let r = rec("s", "/code/zzz", 1, NOW, &[]);
        let kw = r.keywords()[0]; // a synonym of this session's emoji
        idx.upsert(r);
        let out = idx.search(kw, NOW);
        assert_eq!(
            out.first().map(|r| r.id),
            Some(SessionId::from_seed("s")),
            "searching an emoji keyword surfaces the session"
        );
        // keyword tier sits between name and tag.
        assert!(TIER_NAME > TIER_KEYWORD && TIER_KEYWORD > TIER_TAG);
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
