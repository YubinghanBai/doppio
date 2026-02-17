pub mod lru;
pub mod sketch;
pub mod tinylfu;

use std::hash::Hash;

/// Core eviction/admission strategy.
///
/// All methods are called **single-threadedly** by the maintenance path.
/// Implementors only need to be `Send`; `Sync` is not required because
/// the cache wraps the policy in a `Mutex`.
pub trait Policy<K: Hash + Eq>: Send {
    /// Called when an existing entry is accessed (read hit).
    fn on_access(&mut self, key: &K);

    /// Called when a new entry is inserted.
    ///
    /// Returns the keys that must be evicted to stay within capacity.
    fn on_insert(&mut self, key: K, weight: u64) -> Vec<K>;

    /// Called when an existing entry's weight changes (e.g., value replaced).
    ///
    /// Returns the keys that must be evicted to stay within the new capacity.
    fn on_update(&mut self, key: &K, old_weight: u64, new_weight: u64) -> Vec<K>;

    /// Called when an entry is explicitly removed.
    fn on_remove(&mut self, key: &K);

    /// Total weight currently tracked by the policy.
    fn current_weight(&self) -> u64;

    /// Maximum weight allowed.
    fn max_weight(&self) -> u64;

    /// Convenience: returns `true` if `current_weight >= max_weight`.
    fn is_full(&self) -> bool {
        self.current_weight() >= self.max_weight()
    }
}
