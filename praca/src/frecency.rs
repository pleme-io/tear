//! Frecency — recency-weighted frequency ranking, sourced from the fleet
//! frecency primitive.
//!
//! A session's rank is its visit count scaled by how recently it was
//! last touched. A session touched seconds ago outranks one touched a
//! week ago even if the older one was visited more often — recency
//! decays the weight in coarse age buckets so the math is cheap,
//! integer-free of floating drift across platforms, and obvious to
//! reason about.
//!
//! All time is `u64` unix-seconds INJECTED by the caller — this module
//! never reads the clock, so ranking is deterministic and testable.
//!
//! # The curve is not defined here
//!
//! It is [`wadachi_spec::DecayKind::ZoxideLogBuckets`]. This module used to
//! hold its own copy of the thresholds (1h / 1d / 1w) and multipliers
//! (4.0 / 2.0 / 0.5 / 0.25) — byte-identical to wadachi-spec's, in a crate
//! that did not depend on wadachi-spec at all. Two copies of one curve stay
//! equal exactly until the first edit that touches one of them, and nothing
//! anywhere would have failed when that happened.
//!
//! The named instance this module *is* is **`praca-parity`**:
//! `ZoxideLogBuckets` decay, `recency_weight` 1.0, and a **multiplicative**
//! combine — `visits × decay(age of the last visit)` rather than the
//! `Σ decayed + freq_weight × freq` every other instance uses.
//!
//! That difference is deliberate and structural, not drift. praça stores a
//! session as a visit **counter plus one timestamp**
//! ([`crate::SessionRecord`]), not a per-visit log, so `Σ decay(age_i)` has
//! exactly one term to sum and the additive combine cannot express "twelve
//! visits, the last one a minute ago". It is also what zoxide itself
//! computes. The fix went upstream: `wadachi-spec` now carries
//! `CombineKind::FreqTimesLatestDecay` and ships this exact configuration as
//! the named instance `praca-parity`, so the shape is a *selection* and no
//! longer a reason for anyone to fork the curve.
//!
//! ## Named interim
//!
//! `CombineKind`, `FrecencyRankingSpec::praca_parity()` and `score_counted()`
//! land in wadachi-spec **after 0.1.9**, which is the newest version published
//! to crates.io as of 2026-08-09. Until that release, this module selects the
//! decay leg by variant and applies the combine in [`score`]. The destination
//! is one line —
//!
//! ```text
//! FrecencyRankingSpec::by_name("praca-parity")   // the whole instance
//!     .score_counted(visits, age_days)           // decay + combine, upstream
//! ```
//!
//! — reachable by bumping the `wadachi-spec` requirement once that version is
//! on crates.io. The parity test below pins the arithmetic either way, so the
//! flip is verifiable rather than hopeful.

use wadachi_spec::DecayKind;

/// The decay leg of the `praca-parity` instance. The bucket thresholds and
/// multipliers live in `wadachi-spec`, not here — that is the whole point.
const DECAY: DecayKind = DecayKind::ZoxideLogBuckets;

/// The `recency_weight` of the `praca-parity` instance. Named rather than
/// inlined so the local combine below reads as the spec's fields, and so the
/// eventual `score_counted` swap is a deletion rather than a re-derivation.
const RECENCY_WEIGHT: f64 = 1.0;

/// `half_life_days` of the `praca-parity` instance. `ZoxideLogBuckets` does
/// not read it (only `ExpHalfLife` does), but it is a spec field and passing
/// the instance's real value keeps this call honest under a decay change.
const HALF_LIFE_DAYS: f64 = 0.0;

/// Seconds per day — the unit border. praça counts unix-seconds; the fleet
/// primitive's decay is a function of **days**.
///
/// The conversion is exact at every bucket boundary: 3600, 86_400 and 604_800
/// divided by 86_400.0 are the correctly-rounded f64s for 1/24, 1 and 7, which
/// is what `ZoxideLogBuckets` compares against. `boundaries_survive_the_seconds_to_days_border`
/// pins that rather than trusting it.
const SECS_PER_DAY: f64 = 86_400.0;

/// An age in seconds as an age in days — the unit the fleet decay curve takes.
#[allow(clippy::cast_precision_loss)]
fn age_days(age_secs: u64) -> f64 {
    age_secs as f64 / SECS_PER_DAY
}

/// Recency multiplier for an age in seconds, in coarse buckets:
/// `< 1h → ×4`, `< 1d → ×2`, `< 1w → ×0.5`, else `×0.25`.
///
/// Pure, and a projection of [`wadachi_spec::DecayKind::ZoxideLogBuckets`] onto
/// praça's seconds. A future age (`now < last_seen`, e.g. clock skew) is
/// clamped to age 0 by the caller via [`score`]; passed directly here a small
/// age just lands in the freshest bucket.
#[must_use]
pub fn recency_weight(age_secs: u64) -> f64 {
    DECAY.decay(age_days(age_secs), HALF_LIFE_DAYS)
}

/// Frecency score for a session: `visits × recency_weight(now -
/// last_seen)`.
///
/// This is the `praca-parity` combine (`CombineKind::FreqTimesLatestDecay` —
/// see the module docs): praça holds a visit counter and one timestamp, so the
/// count multiplies the last visit's decayed weight instead of summing a
/// per-visit log.
///
/// `now` is expected `>= last_seen`; if `now < last_seen` (clock skew /
/// a future-stamped record) the age saturates to 0 so the session lands
/// in the freshest bucket rather than underflowing.
#[must_use]
pub fn score(visits: u32, last_seen: u64, now: u64) -> f64 {
    let age = now.saturating_sub(last_seen);
    RECENCY_WEIGHT * f64::from(visits) * recency_weight(age)
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOW: u64 = 1_000_000_000;

    /// The bucket boundaries, in seconds. These live in the TEST module on
    /// purpose: production code no longer knows them, because knowing them was
    /// the duplication. Here they are the fixed expectations the fleet curve
    /// has to keep meeting.
    const HOUR: u64 = 3_600;
    /// Seconds in a day.
    const DAY: u64 = 24 * HOUR;
    /// Seconds in a week.
    const WEEK: u64 = 7 * DAY;

    /// The exact pre-de-duplication implementation, preserved ONLY here so the
    /// parity claim is a test rather than a sentence in a commit message. This
    /// is the code that used to sit above `#[cfg(test)]`, character for
    /// character; nothing in production may call it.
    fn legacy_recency_weight(age_secs: u64) -> f64 {
        if age_secs < HOUR {
            4.0
        } else if age_secs < DAY {
            2.0
        } else if age_secs < WEEK {
            0.5
        } else {
            0.25
        }
    }

    /// The pre-de-duplication [`score`], likewise preserved for the proof.
    fn legacy_score(visits: u32, last_seen: u64, now: u64) -> f64 {
        let age = now.saturating_sub(last_seen);
        f64::from(visits) * legacy_recency_weight(age)
    }

    /// THE DE-DUPLICATION PROOF. Sourcing the curve from `wadachi-spec` must
    /// change praça's ranking by exactly nothing, so this asserts **bit
    /// identity** — not a tolerance — between the new path and the deleted
    /// constants, across a spread of (visits, age) inputs that includes every
    /// bucket boundary and both sides of it.
    ///
    /// Exact equality is the right assertion: every multiplier (4.0, 2.0, 0.5,
    /// 0.25) and every visit count is exactly representable, so the product is
    /// exact. A tolerance here would hide a real bucket disagreement.
    #[test]
    fn wadachi_spec_path_is_bit_identical_to_the_deleted_constants() {
        let ages: &[u64] = &[
            0,
            1,
            59,
            HOUR - 1,
            HOUR,
            HOUR + 1,
            2 * HOUR,
            DAY - 1,
            DAY,
            DAY + 1,
            3 * DAY,
            WEEK - 1,
            WEEK,
            WEEK + 1,
            2 * WEEK,
            52 * WEEK,
            10 * 52 * WEEK,
        ];
        let visit_counts: &[u32] = &[0, 1, 2, 3, 7, 20, 999, u32::MAX];

        for &age in ages {
            #[allow(clippy::float_cmp)]
            {
                assert_eq!(
                    recency_weight(age),
                    legacy_recency_weight(age),
                    "recency_weight diverged at age {age}s"
                );
            }
            for &visits in visit_counts {
                let last_seen = NOW - age;
                #[allow(clippy::float_cmp)]
                {
                    assert_eq!(
                        score(visits, last_seen, NOW),
                        legacy_score(visits, last_seen, NOW),
                        "score diverged at visits={visits} age={age}s"
                    );
                }
            }
        }

        // Clock skew: a future-stamped record saturates identically on both
        // paths (age 0 → freshest bucket), rather than underflowing.
        #[allow(clippy::float_cmp)]
        {
            assert_eq!(score(3, NOW + 500, NOW), legacy_score(3, NOW + 500, NOW));
        }
    }

    /// The seconds→days conversion must not move a bucket edge. Each boundary
    /// is the *first* second of the slower bucket, and one second earlier is
    /// still the faster one — which only holds if `n / 86_400.0` lands exactly
    /// on the f64 the curve compares against.
    #[test]
    fn boundaries_survive_the_seconds_to_days_border() {
        #[allow(clippy::float_cmp)]
        {
            assert_eq!(age_days(HOUR), 1.0 / 24.0);
            assert_eq!(age_days(DAY), 1.0);
            assert_eq!(age_days(WEEK), 7.0);
        }
        assert!(age_days(HOUR - 1) < 1.0 / 24.0);
        assert!(age_days(DAY - 1) < 1.0);
        assert!(age_days(WEEK - 1) < 7.0);
    }

    #[test]
    fn weight_buckets_step_down_by_age() {
        assert_eq!(recency_weight(0), 4.0);
        assert_eq!(recency_weight(HOUR - 1), 4.0);
        assert_eq!(recency_weight(HOUR), 2.0);
        assert_eq!(recency_weight(DAY - 1), 2.0);
        assert_eq!(recency_weight(DAY), 0.5);
        assert_eq!(recency_weight(WEEK - 1), 0.5);
        assert_eq!(recency_weight(WEEK), 0.25);
        assert_eq!(recency_weight(WEEK * 52), 0.25);
    }

    #[test]
    fn recent_few_visits_beats_old_many_visits() {
        // 2 visits a minute ago vs 20 visits two weeks ago.
        let recent = score(2, NOW - 60, NOW); // 2 * 4.0 = 8.0
        let stale = score(20, NOW - 2 * WEEK, NOW); // 20 * 0.25 = 5.0
        assert!(recent > stale, "recent {recent} should beat stale {stale}");
    }

    #[test]
    fn more_visits_wins_within_same_bucket() {
        let a = score(3, NOW - 30, NOW);
        let b = score(5, NOW - 30, NOW);
        assert!(b > a);
    }

    #[test]
    fn future_last_seen_saturates_to_freshest() {
        // last_seen in the future -> age 0 -> freshest weight, no panic.
        let s = score(1, NOW + 500, NOW);
        assert_eq!(s, 4.0);
    }

    #[test]
    fn zero_visits_is_zero() {
        assert_eq!(score(0, NOW - 10, NOW), 0.0);
    }

    #[test]
    fn ordering_is_sane_across_buckets() {
        // Build a few sessions and assert the frecency ordering matches
        // intuition: fresh-and-frequent > fresh-but-rare > old-frequent
        // > ancient-rare.
        let fresh_frequent = score(10, NOW - 10, NOW); // 40.0
        let fresh_rare = score(1, NOW - 10, NOW); // 4.0
        let old_frequent = score(10, NOW - 3 * DAY, NOW); // 5.0
        let ancient_rare = score(1, NOW - 60 * DAY, NOW); // 0.25
        assert!(fresh_frequent > old_frequent);
        assert!(old_frequent > fresh_rare);
        assert!(fresh_rare > ancient_rare);
    }
}
