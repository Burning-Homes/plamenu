//! Shared bounding for the process-lifetime TTL caches — the rate limiter's
//! bearer-token resolution cache ([`crate::rate_limit`]) and the admin metrics
//! cache ([`crate::state::MetricsCache`]). Both are plain `Mutex<HashMap>`s;
//! this keeps their advertised maximum honest.

use std::collections::HashMap;
use std::hash::{BuildHasher, Hash};
use std::time::{Duration, Instant};

/// Inserts `(key, value)` into a timestamped cache while guaranteeing the map
/// never holds more than `max` entries.
///
/// The naive "remove entries past their TTL, then insert" pattern does *not*
/// bound a cache under a flood of unique, still-fresh keys: when every entry is
/// within `ttl` the stale sweep frees nothing and the map grows past `max`
/// without limit. This first drops entries older than `ttl`,
/// and if that still leaves the map at capacity — every survivor is fresh, i.e.
/// a unique-key flood — evicts the oldest entries down to a low-water mark so
/// the map stays hard-bounded by `max`.
///
/// The O(n log n) compaction runs only when the map is at capacity, and each
/// run frees `max / 4` slots, so a sustained insert stream amortizes to
/// O(log n) per insert. `now` is a parameter so the eviction is deterministic
/// under test; callers pass [`Instant::now`].
pub fn bounded_ttl_insert<K, V, S>(
    map: &mut HashMap<K, (Instant, V), S>,
    key: K,
    value: V,
    now: Instant,
    ttl: Duration,
    max: usize,
) where
    K: Eq + Hash + Clone,
    S: BuildHasher,
{
    debug_assert!(max >= 4, "bounded_ttl_insert needs headroom to evict");
    if map.len() >= max {
        // Cheap first pass: drop entries past their TTL.
        map.retain(|_, (stored_at, _)| now.saturating_duration_since(*stored_at) < ttl);
        // Still full? Every survivor is fresh (a unique-key flood). Evict the
        // oldest entries down to a low-water mark, leaving room for the insert.
        if map.len() >= max {
            let low_water = max - max / 4;
            let excess = map.len() + 1 - low_water;
            let mut by_age: Vec<(Instant, K)> =
                map.iter().map(|(k, (at, _))| (*at, k.clone())).collect();
            by_age.sort_unstable_by_key(|(at, _)| *at);
            for (_, k) in by_age.into_iter().take(excess) {
                map.remove(&k);
            }
        }
    }
    map.insert(key, (now, value));
}

#[cfg(test)]
mod tests {
    use super::*;

    const TTL: Duration = Duration::from_mins(1);

    #[test]
    fn keeps_all_entries_below_the_cap() {
        let mut map: HashMap<u32, (Instant, ())> = HashMap::new();
        let base = Instant::now();
        for i in 0..8 {
            bounded_ttl_insert(
                &mut map,
                i,
                (),
                base + Duration::from_millis(i.into()),
                TTL,
                16,
            );
        }
        assert_eq!(map.len(), 8);
    }

    #[test]
    fn flood_of_unique_fresh_keys_stays_bounded() {
        // Every key is unique and within the TTL, so the stale sweep can never
        // free a slot — the pre-fix `retain`-then-insert path grew without
        // bound here. The map must stay at or below `max` throughout.
        let mut map: HashMap<u32, (Instant, ())> = HashMap::new();
        let base = Instant::now();
        let max = 64;
        for i in 0..5_000u32 {
            bounded_ttl_insert(
                &mut map,
                i,
                (),
                base + Duration::from_millis(i.into()),
                TTL,
                max,
            );
            assert!(map.len() <= max, "grew to {} at i={i}", map.len());
        }
        // The most recent inserts survive; the oldest were evicted.
        assert!(map.contains_key(&4_999));
        assert!(!map.contains_key(&0));
    }

    #[test]
    fn stale_entries_are_reclaimed_before_eviction() {
        let mut map: HashMap<u32, (Instant, ())> = HashMap::new();
        let base = Instant::now();
        let max = 16usize;
        // Fill to the cap with entries that are already stale relative to `now`.
        for i in 0..u32::try_from(max).unwrap() {
            map.insert(i, (base, ()));
        }
        // Insert well past the TTL: the stale sweep clears everything, so no
        // fresh entry needs age-based eviction.
        bounded_ttl_insert(&mut map, 999, (), base + Duration::from_mins(2), TTL, max);
        assert_eq!(map.len(), 1);
        assert!(map.contains_key(&999));
    }

    #[test]
    fn identical_timestamps_still_stay_bounded() {
        // Coarse clocks can stamp many inserts with the same `Instant`; a purely
        // timestamp-cutoff eviction could then free nothing. Eviction must
        // remove a fixed count regardless of ties.
        let mut map: HashMap<u32, (Instant, ())> = HashMap::new();
        let now = Instant::now();
        let max = 32;
        for i in 0..1_000u32 {
            bounded_ttl_insert(&mut map, i, (), now, TTL, max);
            assert!(map.len() <= max, "grew to {} at i={i}", map.len());
        }
    }
}
