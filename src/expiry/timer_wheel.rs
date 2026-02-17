//! Hierarchical timer wheel for TTL / TTI expiry.
//!
//! ## Algorithm
//!
//! The wheel has **5 levels**, each composed of a fixed number of buckets.
//! Every level covers a geometrically larger time range:
//!
//! | Level | Buckets | Bucket span      | Total range   |
//! |-------|---------|------------------|---------------|
//! | 0     | 64      | ~1.07 s          | ~68 s         |
//! | 1     | 64      | ~68.7 s          | ~73 min       |
//! | 2     | 32      | ~73.1 min        | ~39 hr        |
//! | 3     | 4       | ~9.7 hr          | ~38.6 hr      |
//! | 4     | 1       | ∞  (catch-all)   | unbounded     |
//!
//! Each bucket holds a `Vec` of `(key, expiry_nanos)` pairs.
//!
//! ### Scheduling
//!
//! `schedule(key, expiry_nanos)` finds the *finest* level whose bucket span
//! is larger than `expiry_nanos − now` and inserts the entry there.  If the
//! entry is farther out than level 3 can represent, it lands in level 4.
//!
//! ### Advancing
//!
//! `advance(now_nanos)` walks every level from finest to coarsest.  At each
//! level it processes every bucket whose wall-clock time has passed since the
//! last `advance` call:
//!
//! - **Level 0**: entries in processed buckets are compared against `now`; any
//!   entry with `expiry ≤ now` is collected as expired.  Entries that are not
//!   yet expired are re-scheduled (can happen if a bucket's span is larger
//!   than its granularity, which doesn't occur at level 0 by design).
//! - **Levels 1–4**: entries in processed buckets are *cascaded* — they are
//!   re-inserted into the wheel at the appropriate lower level.
//!
//! The index (`AHashMap<K, u64>`) is the **source of truth** for each key's
//! current expiry time.  When an entry is rescheduled (e.g., TTI resets on
//! read), the index is updated and the old wheel slot is implicitly
//! invalidated — stale entries encountered during advance are silently
//! skipped.
//!
//! ## References
//! - Varghese & Lauck (1987). *Hashed and Hierarchical Timing Wheels.*
//! - Caffeine source: `com.github.benmanes.caffeine.cache.TimerWheel`

use std::hash::Hash;

use ahash::AHashMap;

// ---------------------------------------------------------------------------
// Wheel geometry (powers-of-two bucket spans, matching Caffeine's constants)
// ---------------------------------------------------------------------------

/// Bucket span in nanoseconds for each level.
///
/// Each value is the next power of two above the corresponding real-time unit:
///   Level 0: ceil_pow2(1s)   = 2^30 ≈ 1.07 s
///   Level 1: ceil_pow2(1min) = 2^36 ≈ 68.7 s
///   Level 2: ceil_pow2(1hr)  = 2^42 ≈ 1.17 hr
///   Level 3: ceil_pow2(1day) = 2^48 ≈ 3.27 day
///   Level 4: 4 × level-3 span (catch-all sentinel)
const SPANS: [u64; 6] = [
    1u64 << 30,                // Level 0: ~1.07 s / bucket
    1u64 << 36,                // Level 1: ~68.7 s / bucket
    1u64 << 42,                // Level 2: ~73.1 min / bucket
    1u64 << 48,                // Level 3: ~3.27 day / bucket
    (1u64 << 48).wrapping_mul(4), // Level 4: catch-all (4 × level-3 span)
    (1u64 << 48).wrapping_mul(4), // Sentinel (same as level 4)
];

/// Number of buckets per level.
const BUCKET_COUNTS: [usize; 5] = [64, 64, 32, 4, 1];

// ---------------------------------------------------------------------------
// TimerWheel
// ---------------------------------------------------------------------------

/// A hierarchical timer wheel for scheduling and detecting expired cache entries.
///
/// All times are expressed as **nanoseconds since an arbitrary epoch** — the
/// caller is responsible for passing a consistent clock (e.g.
/// `Instant::now().duration_since(start).as_nanos() as u64`).
pub struct TimerWheel<K> {
    /// `wheels[level][bucket]` → Vec of `(key, expiry_nanos)` pairs.
    wheels: [Vec<Vec<(K, u64)>>; 5],
    /// Wall-clock time at the last `advance` call, in nanos since epoch.
    nanos: u64,
    /// Canonical expiry time for each scheduled key.
    ///
    /// When a key is rescheduled this map is updated.  Stale wheel entries
    /// (whose stored expiry no longer matches this map) are skipped on drain.
    index: AHashMap<K, u64>,
}

impl<K: Hash + Eq + Clone> TimerWheel<K> {
    /// Creates a new wheel anchored at `start_nanos`.
    pub fn new(start_nanos: u64) -> Self {
        TimerWheel {
            wheels: [
                vec![Vec::new(); BUCKET_COUNTS[0]],
                vec![Vec::new(); BUCKET_COUNTS[1]],
                vec![Vec::new(); BUCKET_COUNTS[2]],
                vec![Vec::new(); BUCKET_COUNTS[3]],
                vec![Vec::new(); BUCKET_COUNTS[4]],
            ],
            nanos: start_nanos,
            index: AHashMap::new(),
        }
    }

    // -----------------------------------------------------------------------
    // Scheduling helpers
    // -----------------------------------------------------------------------

    /// Returns `(level, bucket_index)` for an entry expiring at `expiry`.
    fn bucket_for(&self, expiry: u64) -> (usize, usize) {
        let now = self.nanos;
        let delay = expiry.saturating_sub(now);

        for level in 0..5 {
            let span = SPANS[level];
            let max_range = span.saturating_mul(BUCKET_COUNTS[level] as u64);
            if delay < max_range || level == 4 {
                let idx = (expiry / span) as usize & (BUCKET_COUNTS[level] - 1);
                return (level, idx);
            }
        }
        // Unreachable (level 4 is the catch-all), but satisfy the compiler.
        (4, 0)
    }

    /// Inserts `(key, expiry_nanos)` into the wheel at the right bucket.
    fn push(&mut self, key: K, expiry: u64) {
        let (level, bucket) = self.bucket_for(expiry);
        self.wheels[level][bucket].push((key, expiry));
    }

    // -----------------------------------------------------------------------
    // Public API
    // -----------------------------------------------------------------------

    /// Schedules `key` to expire at `expiry_nanos`.
    ///
    /// If the key was already scheduled, the old entry is implicitly
    /// invalidated (the index is updated; the stale wheel entry will be
    /// skipped during the next `advance`).
    pub fn schedule(&mut self, key: K, expiry_nanos: u64) {
        self.index.insert(key.clone(), expiry_nanos);
        self.push(key, expiry_nanos);
    }

    /// Cancels the scheduled expiry for `key`.
    ///
    /// Returns `true` if the key was scheduled, `false` otherwise.
    /// The wheel entry is lazily discarded during the next `advance`.
    pub fn cancel(&mut self, key: &K) -> bool {
        self.index.remove(key).is_some()
    }

    /// Advances the wheel to `now_nanos` and returns all expired keys.
    ///
    /// Entries in passed buckets at level 0 that have `expiry ≤ now` are
    /// collected.  Entries in passed buckets at higher levels are cascaded
    /// down into finer-grained levels.
    pub fn advance(&mut self, now_nanos: u64) -> Vec<K> {
        let mut expired = Vec::new();

        // Process each level from finest to coarsest.
        for level in 0..5 {
            let span = SPANS[level];
            let prev_tick = self.nanos / span;
            let now_tick = now_nanos / span;

            if now_tick <= prev_tick {
                // This level's clock hasn't ticked since last advance; no
                // cascading needed at higher levels either.
                break;
            }

            // Process all buckets whose wall-clock time has passed.
            // Clamp the range to avoid walking more than one full revolution.
            let ticks_to_walk = (now_tick - prev_tick)
                .min(BUCKET_COUNTS[level] as u64);

            for tick_offset in 1..=ticks_to_walk {
                let tick = prev_tick + tick_offset;
                let bucket_idx = (tick as usize) & (BUCKET_COUNTS[level] - 1);
                let entries = std::mem::take(&mut self.wheels[level][bucket_idx]);

                for (key, expiry) in entries {
                    // Check against the index — skip stale entries.
                    match self.index.get(&key) {
                        Some(&canonical_expiry) if canonical_expiry == expiry => {
                            if expiry <= now_nanos {
                                // The entry has expired (may be discovered at any
                                // level when a coarser bucket is processed and the
                                // deadline has already passed).
                                self.index.remove(&key);
                                expired.push(key);
                            } else {
                                // Not yet expired: cascade into the appropriate
                                // finer-grained level so it is checked again on a
                                // future `advance` call.
                                self.push(key, expiry);
                            }
                        }
                        _ => {
                            // Stale or cancelled — discard silently.
                        }
                    }
                }
            }
        }

        self.nanos = now_nanos;
        expired
    }

    /// Returns the number of keys currently tracked (O(1)).
    pub fn len(&self) -> usize {
        self.index.len()
    }

    /// Returns `true` if no keys are scheduled.
    pub fn is_empty(&self) -> bool {
        self.index.is_empty()
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// One second expressed as nanos, using level-0 bucket span for alignment.
    const S: u64 = 1_000_000_000;

    fn wheel() -> TimerWheel<u64> {
        TimerWheel::new(0)
    }

    #[test]
    fn nothing_expires_before_deadline() {
        let mut w = wheel();
        w.schedule(1, 10 * S);
        let out = w.advance(5 * S);
        assert!(out.is_empty(), "should not expire before deadline");
    }

    #[test]
    fn expires_after_deadline() {
        let mut w = wheel();
        w.schedule(42, 10 * S);
        let out = w.advance(11 * S);
        assert_eq!(out, vec![42], "key should expire after deadline");
    }

    #[test]
    fn multiple_keys_expire_correctly() {
        let mut w = wheel();
        w.schedule(1, 5 * S);
        w.schedule(2, 15 * S);
        w.schedule(3, 30 * S);

        let mut expired = w.advance(20 * S);
        expired.sort();
        assert_eq!(expired, vec![1, 2]);

        let mut expired2 = w.advance(35 * S);
        expired2.sort();
        assert_eq!(expired2, vec![3]);
    }

    #[test]
    fn cancel_prevents_expiry() {
        let mut w = wheel();
        w.schedule(7, 10 * S);
        let removed = w.cancel(&7);
        assert!(removed);
        let out = w.advance(20 * S);
        assert!(out.is_empty(), "cancelled entry should not expire");
    }

    #[test]
    fn reschedule_uses_new_expiry() {
        let mut w = wheel();
        w.schedule(5, 10 * S);
        w.schedule(5, 30 * S); // reschedule farther out

        let out1 = w.advance(15 * S);
        assert!(out1.is_empty(), "rescheduled entry should not expire early");

        let out2 = w.advance(35 * S);
        assert_eq!(out2, vec![5], "rescheduled entry should expire at new time");
    }

    #[test]
    fn long_ttl_via_level4_cascade() {
        let mut w = wheel();
        // Schedule something > level-3's range (> 4 × 2^48 ns ≈ 52 days)
        // We use 2 × level-4 span ≈ 26 days here; adjust if spans change.
        let far_future = SPANS[3] * 5 + 1;
        w.schedule(99, far_future);

        // Not expired at half-way.
        let out = w.advance(far_future / 2);
        assert!(out.is_empty());

        // Expired past the deadline.
        let out2 = w.advance(far_future + S);
        assert_eq!(out2, vec![99]);
    }

    #[test]
    fn len_tracks_scheduled_keys() {
        let mut w = wheel();
        assert_eq!(w.len(), 0);
        w.schedule(1, 10 * S);
        w.schedule(2, 20 * S);
        assert_eq!(w.len(), 2);
        w.cancel(&1);
        assert_eq!(w.len(), 1);
        w.advance(25 * S);
        assert_eq!(w.len(), 0);
    }
}
