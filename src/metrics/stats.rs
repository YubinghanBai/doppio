use std::sync::atomic::{AtomicU64, Ordering};

/// Atomic counters updated on every cache operation.
pub struct StatsCounter {
    hits: AtomicU64,
    misses: AtomicU64,
    evictions: AtomicU64,
}

impl StatsCounter {
    pub fn new() -> Self {
        StatsCounter {
            hits: AtomicU64::new(0),
            misses: AtomicU64::new(0),
            evictions: AtomicU64::new(0),
        }
    }

    #[inline]
    pub fn record_hit(&self) {
        self.hits.fetch_add(1, Ordering::Relaxed);
    }

    #[inline]
    pub fn record_miss(&self) {
        self.misses.fetch_add(1, Ordering::Relaxed);
    }

    #[inline]
    pub fn record_eviction(&self, count: u64) {
        self.evictions.fetch_add(count, Ordering::Relaxed);
    }

    /// Returns a point-in-time snapshot of the statistics.
    pub fn snapshot(&self) -> Metrics {
        let hits = self.hits.load(Ordering::Relaxed);
        let misses = self.misses.load(Ordering::Relaxed);
        let evictions = self.evictions.load(Ordering::Relaxed);
        let total = hits + misses;
        let hit_rate = if total == 0 {
            0.0_f64
        } else {
            hits as f64 / total as f64
        };
        Metrics {
            hits,
            misses,
            evictions,
            hit_rate,
        }
    }
}

impl Default for StatsCounter {
    fn default() -> Self {
        Self::new()
    }
}

/// A point-in-time snapshot of cache statistics.
#[derive(Debug, Clone, PartialEq)]
pub struct Metrics {
    /// Number of cache hits (key found).
    pub hits: u64,
    /// Number of cache misses (key not found).
    pub misses: u64,
    /// Number of entries evicted due to capacity pressure.
    pub evictions: u64,
    /// `hits / (hits + misses)`, or `0.0` if no requests have been made.
    pub hit_rate: f64,
}

impl Metrics {
    pub fn request_count(&self) -> u64 {
        self.hits + self.misses
    }
}
