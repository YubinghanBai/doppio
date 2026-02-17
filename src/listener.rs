//! Eviction listener — a callback invoked whenever an entry leaves the cache.
//!
//! # Example
//! ```
//! use doppio::CacheBuilder;
//! use doppio::listener::EvictionCause;
//! use std::sync::{Arc, Mutex};
//!
//! let log: Arc<Mutex<Vec<(u64, EvictionCause)>>> = Arc::new(Mutex::new(Vec::new()));
//! let log2 = Arc::clone(&log);
//!
//! let cache: doppio::Cache<u64, u64> = CacheBuilder::new(2)
//!     .eviction_listener(move |key: &u64, _val, cause| {
//!         log2.lock().unwrap().push((*key, cause));
//!     })
//!     .build();
//!
//! cache.insert(1, 10);
//! cache.insert(2, 20);
//! cache.insert(3, 30); // capacity eviction
//! cache.invalidate(&1); // explicit removal (may already be evicted)
//! ```

use std::sync::Arc;

// ---------------------------------------------------------------------------
// EvictionCause
// ---------------------------------------------------------------------------

/// The reason an entry was removed from the cache.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EvictionCause {
    /// Removed because the cache exceeded its capacity and this entry was
    /// chosen as the victim by the W-TinyLFU admission policy.
    Capacity,
    /// Removed because its TTL or TTI expired.
    Expired,
    /// Removed explicitly via [`Cache::invalidate`] or
    /// [`Cache::invalidate_all`].
    ///
    /// [`Cache::invalidate`]: crate::Cache::invalidate
    /// [`Cache::invalidate_all`]: crate::Cache::invalidate_all
    Explicit,
}

// ---------------------------------------------------------------------------
// EvictionListener trait
// ---------------------------------------------------------------------------

/// A callback invoked each time an entry is evicted or invalidated.
///
/// Implementations must be `Send + Sync + 'static` so the listener can be
/// shared across threads via `Arc`.
///
/// The callback receives:
/// - a reference to the evicted key,
/// - a shared reference to the evicted value (`Arc<V>`),
/// - the reason for removal.
///
/// **Do not call any cache method from inside the listener** — it runs on the
/// maintenance thread while holding internal locks, and re-entering the cache
/// would deadlock.
pub trait EvictionListener<K, V>: Send + Sync + 'static {
    fn on_evict(&self, key: &K, value: Arc<V>, cause: EvictionCause);
}

/// An [`EvictionListener`] backed by a closure.
///
/// Created via [`CacheBuilder::eviction_listener`](crate::CacheBuilder::eviction_listener).
pub struct FnListener<F>(pub F);

impl<K, V, F> EvictionListener<K, V> for FnListener<F>
where
    F: Fn(&K, Arc<V>, EvictionCause) + Send + Sync + 'static,
{
    fn on_evict(&self, key: &K, value: Arc<V>, cause: EvictionCause) {
        (self.0)(key, value, cause)
    }
}
