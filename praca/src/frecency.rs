//! Frecency — recency-weighted frequency ranking (zoxide / wadachi
//! style).
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

/// Seconds in an hour.
const HOUR: u64 = 3_600;
/// Seconds in a day.
const DAY: u64 = 24 * HOUR;
/// Seconds in a week.
const WEEK: u64 = 7 * DAY;

/// Recency multiplier for an age in seconds, in coarse buckets:
/// `< 1h → ×4`, `< 1d → ×2`, `< 1w → ×0.5`, else `×0.25`.
///
/// Pure. A future age (`now < last_seen`, e.g. clock skew) is clamped to
/// age 0 by the caller via [`score`]; passed directly here a small age
/// just lands in the freshest bucket.
#[must_use]
pub fn recency_weight(age_secs: u64) -> f64 {
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

/// Frecency score for a session: `visits × recency_weight(now -
/// last_seen)`.
///
/// `now` is expected `>= last_seen`; if `now < last_seen` (clock skew /
/// a future-stamped record) the age saturates to 0 so the session lands
/// in the freshest bucket rather than underflowing.
#[must_use]
pub fn score(visits: u32, last_seen: u64, now: u64) -> f64 {
    let age = now.saturating_sub(last_seen);
    f64::from(visits) * recency_weight(age)
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOW: u64 = 1_000_000_000;

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
