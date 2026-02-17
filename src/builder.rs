use std::hash::Hash;
use std::time::Duration;
use crate::cache::{Cache, ExpiryConfig};
use crate::listener::{EvictionListener, FnListener};
use crate::weigher::{FnWeigher, UnitWeigher, Weigher};

/// Builder for configuring and constructing a [`Cache`].
///
/// # Example
/// ```
/// use doppio::CacheBuilder;
/// use std::time::Duration;
///
/// let cache: doppio::Cache<String, String> = CacheBuilder::new(1_000)
///     .time_to_live(Duration::from_secs(60))
///     .build();
/// ```
pub struct CacheBuilder<K, V> {
    max_capacity: u64,
    num_shards: usize,
    weigher: Box<dyn Weigher<K, V>>,
    expiry: ExpiryConfig,
    listener: Option<Box<dyn EvictionListener<K, V>>>,
}

impl<K: 'static, V: 'static> CacheBuilder<K, V> {
    pub fn new(max_capacity: u64) -> Self {
        assert!(max_capacity > 0, "max_capacity must be greater than 0");
        CacheBuilder {
            max_capacity,
            num_shards: 64,
            weigher: Box::new(UnitWeigher),
            expiry: ExpiryConfig::None,
            listener: None,
        }
    }

    /// Set the number of internal shards (must be a power of two; default: 64).
    pub fn num_shards(mut self, n: usize) -> Self {
        assert!(n > 0 && n.is_power_of_two(), "num_shards must be a power of two");
        self.num_shards = n;
        self
    }

    /// Each entry expires `ttl` after it was **written** (or replaced).
    pub fn time_to_live(mut self, ttl: Duration) -> Self {
        self.expiry = ExpiryConfig::Ttl(ttl);
        self
    }

    /// Each entry expires `tti` after it was **last accessed**.
    pub fn time_to_idle(mut self, tti: Duration) -> Self {
        self.expiry = ExpiryConfig::Tti(tti);
        self
    }

    /// Register an eviction listener closure.
    ///
    /// The closure is called **synchronously on the maintenance thread** each
    /// time an entry is removed for any reason (capacity, expiry, or explicit
    /// invalidation).  Do **not** call cache methods from within the closure.
    ///
    /// # Example
    /// ```
    /// use doppio::CacheBuilder;
    /// use doppio::listener::EvictionCause;
    ///
    /// let cache: doppio::Cache<u64, u64> = CacheBuilder::new(10)
    ///     .eviction_listener(|key: &u64, _val, cause| {
    ///         println!("evicted key={key} cause={cause:?}");
    ///     })
    ///     .build();
    /// ```
    pub fn eviction_listener<F>(mut self, f: F) -> Self
    where
        F: Fn(&K, std::sync::Arc<V>, crate::listener::EvictionCause) + Send + Sync + 'static,
    {
        self.listener = Some(Box::new(FnListener(f)));
        self
    }

    /// Register an eviction listener via the [`EvictionListener`] trait.
    pub fn eviction_listener_impl<L: EvictionListener<K, V>>(mut self, l: L) -> Self {
        self.listener = Some(Box::new(l));
        self
    }

    /// Set a custom entry weigher via closure.
    ///
    /// # Example
    /// ```
    /// use doppio::CacheBuilder;
    ///
    /// let cache: doppio::Cache<String, Vec<u8>> = CacheBuilder::new(4096)
    ///     .weigher(|_k: &String, v: &Vec<u8>| v.len() as u64 + 1)
    ///     .build();
    /// ```
    pub fn weigher<F>(mut self, f: F) -> Self
    where
        F: Fn(&K, &V) -> u64 + Send + Sync + 'static,
    {
        self.weigher = Box::new(FnWeigher(f));
        self
    }

    /// Set a weigher using any type that implements the [`Weigher`] trait.
    pub fn weigher_impl<W: Weigher<K, V>>(mut self, w: W) -> Self {
        self.weigher = Box::new(w);
        self
    }
}

impl<K, V> CacheBuilder<K, V>
where
    K: Hash + Eq + Clone + Send + Sync + 'static,
    V: Send + Sync + 'static,
{
    pub fn build(self) -> Cache<K, V> {
        Cache::new(
            self.max_capacity,
            self.num_shards,
            self.weigher,
            self.expiry,
            self.listener,
        )
    }
}
