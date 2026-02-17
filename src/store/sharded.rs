use std::hash::Hash;
use std::sync::Arc;
use std::time::Instant;
use ahash::{AHashMap, RandomState};
use parking_lot::RwLock;

// ---------------------------------------------------------------------------
// StoreEntry
// ---------------------------------------------------------------------------

/// A single entry in the store.
///
/// `expires_at = None` means the entry never expires.  The cache layer
/// checks this field inline on every `get` so that TTL/TTI enforcement
/// requires **no additional lock** beyond the shard's existing read-lock.
pub struct StoreEntry<V> {
    pub value: Arc<V>,
    /// Absolute expiry time.  `None` = immortal.
    pub expires_at: Option<Instant>,
}

// ---------------------------------------------------------------------------
// Shard
// ---------------------------------------------------------------------------

/// Cache-line padding to prevent false sharing between shards.
#[repr(align(64))]
pub(crate) struct Shard<K, V> {
    pub(crate) map: RwLock<AHashMap<K, StoreEntry<V>>>,
}

// ---------------------------------------------------------------------------
// ShardedStore
// ---------------------------------------------------------------------------

/// A thread-safe key-value store backed by `N` independently-locked shards.
///
/// Reads use a shared lock, writes use an exclusive lock, both per-shard.
pub struct ShardedStore<K, V> {
    shards: Box<[Shard<K, V>]>,
    /// Always `shards.len() - 1`; shards.len() is a power of two.
    shard_mask: usize,
    /// Hasher used only to compute shard indices.
    build_hasher: RandomState,
}

impl<K: Hash + Eq + Clone, V> ShardedStore<K, V> {
    pub fn new(num_shards: usize) -> Self {
        assert!(num_shards.is_power_of_two());
        let shards = (0..num_shards)
            .map(|_| Shard {
                map: RwLock::new(AHashMap::new()),
            })
            .collect::<Vec<_>>()
            .into_boxed_slice();

        ShardedStore {
            shards,
            shard_mask: num_shards - 1,
            build_hasher: RandomState::new(),
        }
    }

    #[inline]
    fn shard_index(&self, key: &K) -> usize {
        let h = self.build_hasher.hash_one(key);
        // Use the high bits (better avalanche from ahash).
        ((h >> 32) as usize) & self.shard_mask
    }

    // -----------------------------------------------------------------------
    // Core operations
    // -----------------------------------------------------------------------

    /// Returns the full entry for `key`, or `None` if absent.
    ///
    /// The caller is responsible for checking `expires_at` and performing
    /// expiry eviction if necessary.
    pub fn get_entry(&self, key: &K) -> Option<(Arc<V>, Option<Instant>)> {
        let idx = self.shard_index(key);
        self.shards[idx]
            .map
            .read()
            .get(key)
            .map(|e| (Arc::clone(&e.value), e.expires_at))
    }

    /// Convenience wrapper that ignores expiry.  Used by callers that don't
    /// care about TTL (e.g. internal maintenance code).
    pub fn get(&self, key: &K) -> Option<Arc<V>> {
        self.get_entry(key).map(|(v, _)| v)
    }

    /// Inserts `value` for `key` with an optional absolute expiry time.
    ///
    /// Returns the previous value, if any.
    pub fn insert(&self, key: K, value: V, expires_at: Option<Instant>) -> Option<Arc<V>> {
        let idx = self.shard_index(&key);
        self.shards[idx]
            .map
            .write()
            .insert(
                key,
                StoreEntry {
                    value: Arc::new(value),
                    expires_at,
                },
            )
            .map(|old| old.value)
    }

    /// Updates the `expires_at` for an existing entry.
    ///
    /// No-op if the key is not present.  Used by the TTI path to reset the
    /// idle timer after a successful read.
    pub fn update_expiry(&self, key: &K, expires_at: Instant) {
        let idx = self.shard_index(key);
        if let Some(entry) = self.shards[idx].map.write().get_mut(key) {
            entry.expires_at = Some(expires_at);
        }
    }

    /// Removes the entry for `key`.  Returns the removed value, if any.
    pub fn remove(&self, key: &K) -> Option<Arc<V>> {
        let idx = self.shard_index(key);
        self.shards[idx].map.write().remove(key).map(|e| e.value)
    }

    /// Returns `true` if the key is present.
    pub fn contains(&self, key: &K) -> bool {
        let idx = self.shard_index(key);
        self.shards[idx].map.read().contains_key(key)
    }

    /// Returns the total number of entries across all shards.
    pub fn len(&self) -> usize {
        self.shards.iter().map(|s| s.map.read().len()).sum()
    }

    pub fn is_empty(&self) -> bool {
        self.shards.iter().all(|s| s.map.read().is_empty())
    }

    /// Removes all entries from every shard.
    pub fn clear(&self) {
        for shard in self.shards.iter() {
            shard.map.write().clear();
        }
    }

    /// Returns a reference to the raw shard slice.
    ///
    /// Used by `invalidate_all` to iterate all keys when firing the listener.
    pub(crate) fn shards(&self) -> &[Shard<K, V>] {
        &self.shards
    }
}
