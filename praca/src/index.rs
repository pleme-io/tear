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
    let all: Vec<Ranked<'a>> = records
        .iter()
        .map(Ranked::Live)
        .chain(defs.iter().map(Ranked::Latent))
        .collect();
    rank_mixed(all, query, now)
}

/// Rank a pre-built heterogeneous list of [`Ranked`] candidates by the
/// picker's order — the heterogeneous core both [`rank_union`] and the
/// picker projection ([`crate::picker::union_view`]) share. Empty query →
/// frecency; non-empty → tier/quality/frecency/[`Searchable::rank_key`].
#[must_use]
pub fn rank_mixed<'a>(all: Vec<Ranked<'a>>, query: &str, now: u64) -> Vec<Ranked<'a>> {
    use std::cmp::Ordering::Equal;
    let q = query.trim();
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
        .filter_map(|it| {
            best_match(q, it.searchable()).map(|m| (m, frec(it.searchable(), now), it))
        })
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

/// Case-insensitive fuzzy scorer — **optimal** alignment by DP, **multi-term**
/// AND matching.
///
/// The needle is split on whitespace into terms; **every** term must match
/// `haystack` (fzf-style AND, order-independent) for the whole to match, and
/// the score is their sum. A single-term needle is the plain subsequence
/// score. This is what makes a query like `mado pr` work — as one literal
/// subsequence it would demand a space in the haystack and match almost
/// nothing; as two terms it finds rows carrying both.
///
/// Each term returns `Some(score)` iff its chars appear in `haystack` in
/// order. Higher is better. Unlike a greedy left-to-right scan (which locks
/// onto the *first* occurrence of each char and can miss a tighter alignment
/// later), each term is scored by a Smith-Waterman-style dynamic program that
/// reports the maximum-scoring alignment — the ranking quality the Ctrl-S
/// picker depends on.
///
/// Rewards (fzf-style fixed bonuses, the model that makes contiguity dominate):
/// * `MATCH` per matched char,
/// * `CONSEC` for a char matched immediately after the previous char
///   (contiguous run),
/// * `FIRST` at the haystack start, `BOUNDARY` right after a separator
///   (`/`, `-`, `_`, `.`, space) — word boundaries,
/// and a light length penalty (applied once) so a tight match on a short
/// field outranks the same subsequence buried in a long path.
///
/// **Smart-case** (fzf/telescope convention): a term with NO uppercase char is
/// matched case-insensitively (forgiving — `pleme` finds `PLEME-42`); a term
/// that contains an uppercase char is matched case-SENSITIVELY (`PLEME` targets
/// the upper-case key precisely, `Api` won't match `api`). The opt-in is the
/// user typing an uppercase char, so there's no surprise. Each whitespace term
/// decides its own case mode.
///
/// An empty / whitespace-only needle scores 0 against any haystack (matches
/// everything) — callers route the empty-query case to frecency-only before
/// reaching here.
#[must_use]
pub fn fuzzy_score(needle: &str, haystack: &str) -> Option<i32> {
    shibori::fuzzy_score(needle, haystack, shibori::Policy::smart())
}

/// Like [`fuzzy_score`], but also returns the haystack CHAR positions that
/// matched in the best alignment — the substrate for matched-character
/// highlighting in the picker. Same smart-case + multi-term semantics; the
/// returned positions are sorted, de-duplicated char indices into `haystack`
/// (so a UI can bold/tint exactly the chars the query hit). The score is
/// identical to [`fuzzy_score`] (a parity test guards the two against drift).
///
/// Caveat: for a case-insensitive term the positions are recovered against the
/// lowercased haystack; when `to_lowercase` changes the char count (rare,
/// non-ASCII) they may shift — highlighting is cosmetic, so this is best-effort.
#[must_use]
pub fn fuzzy_indices(needle: &str, haystack: &str) -> Option<(i32, Vec<usize>)> {
    let needle = needle.trim();
    if needle.is_empty() {
        return Some((0, Vec::new()));
    }
    let hay_orig: Vec<char> = haystack.chars().collect();
    let hay_lc: Vec<char> = haystack.to_lowercase().chars().collect();
    let mut total = 0i32;
    let mut positions: Vec<usize> = Vec::new();
    let mut saw_term = false;
    for term in needle.split_whitespace() {
        if term.is_empty() {
            continue;
        }
        saw_term = true;
        let (n, hay): (Vec<char>, &[char]) = if term.chars().any(char::is_uppercase) {
            (term.chars().collect(), &hay_orig)
        } else {
            (term.to_lowercase().chars().collect(), &hay_lc)
        };
        let (s, idxs) = align_indices(&n, hay)?;
        total += s;
        positions.extend(idxs);
    }
    if !saw_term {
        return Some((0, Vec::new()));
    }
    positions.sort_unstable();
    positions.dedup();
    let m = i32::try_from(hay_orig.len()).unwrap_or(i32::MAX);
    Some((total - m / 16, positions))
}

/// [`align_score`] with backpointers: returns the raw score AND the matched
/// haystack char indices of the best alignment. Separate from `align_score`
/// (which stays a lean two-row DP for the hot ranking path) — this allocates a
/// full DP table to reconstruct positions, only for the visible rows being
/// highlighted.
fn align_indices(n: &[char], hay: &[char]) -> Option<(i32, Vec<usize>)> {
    if n.is_empty() {
        return Some((0, Vec::new()));
    }
    let m = hay.len();
    if m < n.len() {
        return None;
    }

    const MATCH: i32 = 1;
    const CONSEC: i32 = 4;
    const BOUNDARY: i32 = 6;
    const FIRST: i32 = 8;
    const MISS: i32 = i32::MIN / 4;

    let is_sep = |c: char| matches!(c, '/' | '-' | '_' | '.' | ' ');
    let boundary_bonus = |j: usize| -> i32 {
        if j == 0 {
            FIRST
        } else if is_sep(hay[j - 1]) {
            BOUNDARY
        } else {
            0
        }
    };

    // score[i][j] = best score with n[i] placed at hay[j]; back[i][j] = the
    // haystack index n[i-1] was placed at on that best path (usize::MAX for i=0).
    let mut score = vec![vec![MISS; m]; n.len()];
    let mut back = vec![vec![usize::MAX; m]; n.len()];
    for (j, &hc) in hay.iter().enumerate() {
        if hc == n[0] {
            score[0][j] = MATCH + boundary_bonus(j);
        }
    }
    for i in 1..n.len() {
        let mut best_nc_val = MISS;
        let mut best_nc_idx = usize::MAX;
        for j in i..m {
            if j >= 2 && score[i - 1][j - 2] > best_nc_val {
                best_nc_val = score[i - 1][j - 2];
                best_nc_idx = j - 2;
            }
            if hay[j] == n[i] {
                // Prefer the contiguous predecessor on ties (tighter run).
                let (mut pred_val, mut pred_idx) = (best_nc_val, best_nc_idx);
                if score[i - 1][j - 1] > MISS / 2 && score[i - 1][j - 1] + CONSEC >= pred_val {
                    pred_val = score[i - 1][j - 1] + CONSEC;
                    pred_idx = j - 1;
                }
                if pred_val > MISS / 2 {
                    score[i][j] = pred_val + MATCH + boundary_bonus(j);
                    back[i][j] = pred_idx;
                }
            }
        }
    }
    let last = n.len() - 1;
    let (mut best, mut best_j) = (MISS, usize::MAX);
    for j in 0..m {
        if score[last][j] > best {
            best = score[last][j];
            best_j = j;
        }
    }
    if best <= MISS / 2 {
        return None;
    }
    let mut idxs = Vec::with_capacity(n.len());
    let (mut i, mut j) = (last, best_j);
    loop {
        idxs.push(j);
        if i == 0 {
            break;
        }
        j = back[i][j];
        i -= 1;
    }
    idxs.reverse();
    Some((best, idxs))
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
        let mut def =
            SessionDefinition::single_pane("/code/preset-proj", "/bin/zsh", NameStyle::Emoji, 100);
        def.visits = 50;
        let ranked = rank_union(
            std::slice::from_ref(&live),
            std::slice::from_ref(&def),
            "",
            100,
        );
        assert_eq!(ranked.len(), 2);
        assert!(
            matches!(ranked[0], Ranked::Latent(_)),
            "high-frecency preset ranks first"
        );
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
        let def = SessionDefinition::single_pane(
            "/code/bravo-substrate",
            "/bin/zsh",
            NameStyle::Emoji,
            100,
        );
        // "bravo" matches the def's path (TIER_PATH) but not the live one.
        let ranked = rank_union(
            std::slice::from_ref(&live),
            std::slice::from_ref(&def),
            "bravo",
            100,
        );
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
        assert!(
            name_tier > tag_tier,
            "a name match must outrank a tag match"
        );
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
        assert_eq!(
            out[0].id,
            SessionId::from_seed("a"),
            "tighter match ranks first"
        );
    }

    #[test]
    fn fuzzy_subsequence_matches_non_contiguous() {
        // "dpl" is a subsequence of "deploy".
        assert!(fuzzy_score("dpl", "deploy").is_some());
        // "xyz" is not.
        assert!(fuzzy_score("xyz", "deploy").is_none());
    }

    #[test]
    fn fuzzy_lowercase_query_is_case_insensitive() {
        // Smart-case: a lowercase query matches regardless of haystack case.
        // (An uppercase query is precise — see `fuzzy_smart_case`.)
        assert!(fuzzy_score("mado", "code/MADO").is_some());
        assert!(fuzzy_score("mado", "code/mado").is_some());
    }

    #[test]
    fn fuzzy_picks_the_optimal_alignment_not_the_greedy_one() {
        // "ab" appears spread out early (a@0 … b@4) AND contiguous late (a@3 b@4).
        // A greedy scan locks onto a@0 then b@4 (run broken). The DP must find
        // the contiguous "ab" and score it HIGHER than a pure-spread haystack.
        let contiguous = fuzzy_score("ab", "ax_ab").unwrap();
        let spread = fuzzy_score("ab", "axxxb").unwrap();
        assert!(
            contiguous > spread,
            "optimal alignment must reward the contiguous run: {contiguous} vs {spread}"
        );
    }

    #[test]
    fn fuzzy_contiguous_beats_scattered_same_length() {
        // Same haystack length, same matched chars — the run on a word boundary
        // must outscore a scattered match.
        let run = fuzzy_score("api", "x/apiabc").unwrap(); // contiguous after '/'
        let scattered = fuzzy_score("api", "axpxixz").unwrap(); // a..p..i spread
        assert!(run > scattered, "{run} vs {scattered}");
    }

    #[test]
    fn fuzzy_boundary_and_start_bonuses_apply() {
        // Start-of-string match beats a mid-word match of the same needle.
        let at_start = fuzzy_score("dep", "deploy").unwrap();
        let mid_word = fuzzy_score("dep", "xxdeploy").unwrap();
        assert!(at_start > mid_word, "start bonus: {at_start} vs {mid_word}");
        // A match right after a separator beats one buried mid-word.
        let after_sep = fuzzy_score("api", "svc/api").unwrap();
        let buried = fuzzy_score("api", "svcxapi").unwrap();
        assert!(
            after_sep > buried,
            "boundary bonus: {after_sep} vs {buried}"
        );
    }

    #[test]
    fn fuzzy_shorter_haystack_wins_ties() {
        // Same alignment quality; the tighter (shorter) field wins via the
        // length penalty.
        let tight = fuzzy_score("api", "api").unwrap();
        let loose = fuzzy_score("api", "api/very/long/buried/path/segment").unwrap();
        assert!(tight > loose, "{tight} vs {loose}");
    }

    #[test]
    fn fuzzy_empty_needle_matches_everything() {
        assert_eq!(fuzzy_score("", "anything"), Some(0));
        assert_eq!(fuzzy_score("", ""), Some(0));
        // Whitespace-only is treated as empty (matches everything).
        assert_eq!(fuzzy_score("   ", "anything"), Some(0));
    }

    #[test]
    fn fuzzy_multi_term_is_and_and_order_independent() {
        // Both terms present (in either order) → match. A space-separated
        // query is the common Ctrl-S case; as one literal subsequence it would
        // demand a space in the haystack and match almost nothing.
        assert!(fuzzy_score("mado pr", "pr#7 fix mado").is_some());
        assert!(fuzzy_score("pr mado", "pr#7 fix mado").is_some());
        // A missing term fails the whole match (AND, not OR).
        assert!(fuzzy_score("mado pr", "pr#7 fix tear").is_none());
        assert!(fuzzy_score("mado zzz", "pr#7 fix mado").is_none());
        // Each term may be a subsequence, not just a substring.
        assert!(fuzzy_score("md pr", "pr#7 fix mado").is_some());
    }

    #[test]
    fn fuzzy_multi_term_outranks_single_term() {
        // Two satisfied terms accumulate score over a single term on the same
        // haystack, so a more-specific query ranks its target higher.
        let hay = "deploy-mado-service";
        let one = fuzzy_score("mado", hay).unwrap();
        let two = fuzzy_score("mado deploy", hay).unwrap();
        assert!(
            two > one,
            "more matched terms → higher score: {two} vs {one}"
        );
    }

    #[test]
    fn fuzzy_indices_marks_the_matched_chars() {
        // "dpl" in "deploy" (d e p l o y): d@0, p@2, l@3.
        let (_, idx) = fuzzy_indices("dpl", "deploy").unwrap();
        assert_eq!(idx, vec![0, 2, 3]);
        // Contiguous run "api" in "x/api": a@2, p@3, i@4.
        let (_, idx) = fuzzy_indices("api", "x/api").unwrap();
        assert_eq!(idx, vec![2, 3, 4]);
        // No match → None.
        assert!(fuzzy_indices("zzz", "deploy").is_none());
        // Empty needle → empty positions.
        assert_eq!(fuzzy_indices("", "deploy"), Some((0, vec![])));
    }

    #[test]
    fn fuzzy_indices_multi_term_unions_positions() {
        // Both terms contribute their matched positions, sorted + deduped.
        let (_, idx) = fuzzy_indices("pr mado", "pr#7 mado").unwrap();
        // "pr" → 0,1 ; "mado" → 5,6,7,8.
        assert_eq!(idx, vec![0, 1, 5, 6, 7, 8]);
    }

    #[test]
    fn fuzzy_indices_score_matches_fuzzy_score() {
        // Parity guard: the highlight path and the ranking path must never
        // disagree on the score (they share the same bonus model).
        let cases = [
            ("dpl", "deploy"),
            ("api", "svc/api"),
            ("mado pr", "pr#7 fix mado"),
            ("PLEME", "PLEME-42"),
            ("xyz", "deploy"),
            ("", "anything"),
            ("ab", "ax_ab"),
        ];
        for (needle, hay) in cases {
            let s = fuzzy_score(needle, hay);
            let si = fuzzy_indices(needle, hay).map(|(sc, _)| sc);
            assert_eq!(s, si, "score parity for ({needle:?}, {hay:?})");
        }
    }

    #[test]
    fn fuzzy_smart_case() {
        // Lowercase term → case-insensitive (forgiving): finds any case.
        assert!(fuzzy_score("api", "API").is_some());
        assert!(fuzzy_score("api", "api").is_some());
        assert!(fuzzy_score("pleme", "PLEME-42").is_some());
        // A term WITH an uppercase char → case-sensitive (precise).
        assert!(fuzzy_score("PLEME", "PLEME-42").is_some());
        assert!(
            fuzzy_score("API", "api").is_none(),
            "uppercase query is precise"
        );
        assert!(
            fuzzy_score("Api", "api").is_none(),
            "any uppercase ⇒ sensitive"
        );
        // Per-term: a lowercase term stays forgiving while an uppercase term in
        // the same query is precise.
        assert!(fuzzy_score("PLEME parser", "PLEME-42 fix the parser").is_some());
        assert!(
            fuzzy_score("PLEME PARSER", "PLEME-42 fix the parser").is_none(),
            "the uppercase 'PARSER' term can't match lowercase 'parser'"
        );
    }

    #[test]
    fn fuzzy_extra_internal_whitespace_is_ignored() {
        // Collapsed/duplicated spaces don't change the term set.
        assert_eq!(
            fuzzy_score("mado   pr", "pr#7 mado"),
            fuzzy_score("mado pr", "pr#7 mado")
        );
    }

    #[test]
    fn fuzzy_needle_longer_than_haystack_is_none() {
        assert!(fuzzy_score("abcdef", "abc").is_none());
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
