//! Entry weigher — assigns a cost (weight) to each cached entry.
//!
//! The cache enforces `Σ weight(entry) ≤ max_capacity`.  By default every
//! entry costs 1 unit (`UnitWeigher`), so `max_capacity` is simply the
//! maximum number of entries.  A custom weigher allows the cache to bound
//! memory consumption instead of entry count.
//!
//! # Example
//! ```
//! use doppio::CacheBuilder;
//!
//! // Cap at ~10 MB total value size (keys are not counted).
//! let cache: doppio::Cache<String, Vec<u8>> = CacheBuilder::new(10 * 1024 * 1024)
//!     .weigher(|_key: &String, val: &Vec<u8>| val.len() as u64 + 1)
//!     .build();
//! ```

/// Computes the cost of a cache entry.
///
/// The returned weight **must be ≥ 1**.  Returning 0 is treated as 1 to
/// prevent entries from escaping capacity accounting.
pub trait Weigher<K, V>: Send + Sync + 'static {
    fn weigh(&self, key: &K, value: &V) -> u64;
}

// ---------------------------------------------------------------------------
// Built-in implementations
// ---------------------------------------------------------------------------

/// Every entry costs exactly 1 unit.  This is the default weigher.
pub struct UnitWeigher;

impl<K, V> Weigher<K, V> for UnitWeigher {
    #[inline]
    fn weigh(&self, _key: &K, _value: &V) -> u64 {
        1
    }
}

/// A weigher backed by a closure.
///
/// Created via [`CacheBuilder::weigher`](crate::CacheBuilder::weigher).
pub struct FnWeigher<F>(pub F);

impl<K, V, F> Weigher<K, V> for FnWeigher<F>
where
    F: Fn(&K, &V) -> u64 + Send + Sync + 'static,
{
    #[inline]
    fn weigh(&self, key: &K, value: &V) -> u64 {
        (self.0)(key, value).max(1)
    }
}
