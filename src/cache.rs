use std::hash::Hash;
use std::sync::Arc;
use std::time::{Duration, Instant};
use parking_lot::Mutex;

use crate::builder::CacheBuilder;
use crate::buffer::read::StripedReadBuffer;
use crate::buffer::write::{WriteBuffer, WriteOp};
use crate::expiry::timer_wheel::TimerWheel;
use crate::listener::{EvictionCause, EvictionListener};
use crate::metrics::stats::{Metrics, StatsCounter};
use crate::policy::tinylfu::WTinyLfuPolicy;
use crate::policy::Policy;
use crate::store::sharded::ShardedStore;
use crate::weigher::Weigher;

use ahash::RandomState;

// ---------------------------------------------------------------------------
// Expiry configuration
// ---------------------------------------------------------------------------

/// How entries expire, if at all.
#[derive(Clone, Copy, Debug)]
pub enum ExpiryConfig {
    /// No expiry — entries live until evicted by the admission policy.
    None,
    /// TTL: entries expire `d` after they were **written** (or replaced).
    Ttl(Duration),
    /// TTI: entries expire `d` after they were **last accessed**.
    Tti(Duration),
}

impl ExpiryConfig {
    pub fn is_none(&self) -> bool {
        matches!(self, ExpiryConfig::None)
    }
}

// ---------------------------------------------------------------------------
// Cache interior
// ---------------------------------------------------------------------------

/// Shared interior of a [`Cache`].
pub(crate) struct Inner<K, V> {
    pub(crate) store: ShardedStore<K, V>,
    pub(crate) policy: Mutex<WTinyLfuPolicy<K>>,
    pub(crate) build_hasher: RandomState,
    pub(crate) weigher: Box<dyn Weigher<K, V>>,
    pub(crate) expiry: ExpiryConfig,
    pub(crate) timer: Mutex<TimerWheel<K>>,
    pub(crate) epoch: Instant,
    /// Optional eviction listener.  `None` if the user didn't register one.
    pub(crate) listener: Option<Box<dyn EvictionListener<K, V>>>,
    pub(crate) read_buf: StripedReadBuffer,
    pub(crate) write_buf: WriteBuffer<K>,
    pub(crate) maintain_lock: Mutex<()>,
    pub(crate) metrics: StatsCounter,
}

// ---------------------------------------------------------------------------
// Cache handle
// ---------------------------------------------------------------------------

/// A concurrent in-memory cache using the W-TinyLFU admission policy.
///
/// # Example
/// ```
/// use doppio::CacheBuilder;
/// use std::sync::Arc;
///
/// let cache: doppio::Cache<String, String> = doppio::CacheBuilder::new(100).build();
/// cache.insert("hello".to_string(), "world".to_string());
/// assert_eq!(cache.get(&"hello".to_string()), Some(std::sync::Arc::new("world".to_string())));
/// ```
pub struct Cache<K, V> {
    inner: Arc<Inner<K, V>>,
}

impl<K, V> Clone for Cache<K, V> {
    fn clone(&self) -> Self {
        Cache {
            inner: Arc::clone(&self.inner),
        }
    }
}

impl<K, V> Cache<K, V>
where
    K: Hash + Eq + Clone + Send + Sync + 'static,
    V: Send + Sync + 'static,
{
    pub(crate) fn new(
        max_capacity: u64,
        num_shards: usize,
        weigher: Box<dyn Weigher<K, V>>,
        expiry: ExpiryConfig,
        listener: Option<Box<dyn EvictionListener<K, V>>>,
    ) -> Self {
        let hasher = RandomState::new();
        let policy = WTinyLfuPolicy::new_with_hasher(max_capacity, hasher.clone());
        let epoch = Instant::now();
        Cache {
            inner: Arc::new(Inner {
                store: ShardedStore::new(num_shards),
                policy: Mutex::new(policy),
                build_hasher: hasher,
                weigher,
                expiry,
                timer: Mutex::new(TimerWheel::new(0)),
                epoch,
                listener,
                read_buf: StripedReadBuffer::new(),
                write_buf: WriteBuffer::new(),
                maintain_lock: Mutex::new(()),
                metrics: StatsCounter::new(),
            }),
        }
    }

    /// Returns a [`CacheBuilder`] for constructing a new cache.
    pub fn builder(max_capacity: u64) -> CacheBuilder<K, V> {
        CacheBuilder::new(max_capacity)
    }

    // -----------------------------------------------------------------------
    // Time helpers
    // -----------------------------------------------------------------------

    #[inline]
    fn now_nanos(&self) -> u64 {
        self.inner.epoch.elapsed().as_nanos() as u64
    }

    #[inline]
    fn expiry_instant(&self) -> Option<Instant> {
        match self.inner.expiry {
            ExpiryConfig::None => None,
            ExpiryConfig::Ttl(d) | ExpiryConfig::Tti(d) => Some(Instant::now() + d),
        }
    }

    #[inline]
    fn expiry_nanos_from_now(&self) -> Option<u64> {
        match self.inner.expiry {
            ExpiryConfig::None => None,
            ExpiryConfig::Ttl(d) | ExpiryConfig::Tti(d) => {
                Some(self.now_nanos() + d.as_nanos() as u64)
            }
        }
    }

    #[inline]
    fn instant_to_nanos(&self, instant: Instant) -> u64 {
        instant
            .checked_duration_since(self.inner.epoch)
            .unwrap_or(Duration::ZERO)
            .as_nanos() as u64
    }

    // -----------------------------------------------------------------------
    // Hot-path: get
    // -----------------------------------------------------------------------

    /// Returns the value for `key` if it exists and has not expired.
    pub fn get(&self, key: &K) -> Option<Arc<V>> {
        let Some((value, expires_at)) = self.inner.store.get_entry(key) else {
            self.inner.metrics.record_miss();
            return None;
        };

        // Inline expiry check — no timer-wheel lock required.
        if let Some(deadline) = expires_at {
            if Instant::now() >= deadline {
                self.inner.store.remove(key);
                let _ = self.inner.write_buf.push(WriteOp::Remove { key: key.clone() });
                self.inner.metrics.record_miss();
                // Fire listener for the expired entry.
                if let Some(listener) = &self.inner.listener {
                    listener.on_evict(key, value, EvictionCause::Expired);
                }
                return None;
            }
        }

        self.inner.metrics.record_hit();

        // Frequency update (lock-free).
        let h = self.inner.build_hasher.hash_one(key);
        if !self.inner.read_buf.offer(h) {
            self.try_maintain();
        }

        // TTI: immediately update the store deadline; timer wheel is async.
        if let ExpiryConfig::Tti(tti) = self.inner.expiry {
            let new_expiry = Instant::now() + tti;
            self.inner.store.update_expiry(key, new_expiry);
            let _ = self.inner.write_buf.push(WriteOp::Reschedule {
                key: key.clone(),
                expires_at: new_expiry,
            });
        }

        Some(value)
    }

    // -----------------------------------------------------------------------
    // Hot-path: insert
    // -----------------------------------------------------------------------

    /// Inserts `value` for `key`.  If the key already exists the value is
    /// replaced.  Eviction may occur to stay within capacity.
    pub fn insert(&self, key: K, value: V) {
        let weight = self.inner.weigher.weigh(&key, &value).max(1);
        let expires_at = self.expiry_instant();
        let expiry_nanos = self.expiry_nanos_from_now();

        let old_val = self.inner.store.insert(key.clone(), value, expires_at);

        let op = if old_val.is_some() {
            WriteOp::Update {
                key,
                old_weight: weight,
                new_weight: weight,
                expires_at,
            }
        } else {
            WriteOp::Add {
                key,
                weight,
                expires_at: expiry_nanos.map(|_| expires_at.unwrap()),
            }
        };
        let _ = expiry_nanos; // consumed via op variants above

        match self.inner.write_buf.push(op) {
            Ok(()) => self.try_maintain(),
            Err(op) => {
                let evicted = self.apply_write_op_sync(op);
                self.dispatch_evictions(evicted, EvictionCause::Capacity);
            }
        }
    }

    // -----------------------------------------------------------------------
    // Hot-path: invalidate
    // -----------------------------------------------------------------------

    /// Removes the entry for `key`, if present.
    pub fn invalidate(&self, key: &K) {
        if let Some(value) = self.inner.store.remove(key) {
            if !self.inner.expiry.is_none() {
                self.inner.timer.lock().cancel(key);
            }
            let op = WriteOp::Remove { key: key.clone() };
            if let Err(op) = self.inner.write_buf.push(op) {
                let extra = self.apply_write_op_sync(op);
                self.dispatch_evictions(extra, EvictionCause::Capacity);
            }
            if let Some(listener) = &self.inner.listener {
                listener.on_evict(key, value, EvictionCause::Explicit);
            }
        }
    }

    // -----------------------------------------------------------------------
    // Maintenance
    // -----------------------------------------------------------------------

    fn try_maintain(&self) {
        let Some(_guard) = self.inner.maintain_lock.try_lock() else { return };
        self.do_maintain();
    }

    fn do_maintain(&self) {
        let mut hashes: Vec<u64> = Vec::new();
        let mut write_ops: Vec<WriteOp<K>> = Vec::new();
        self.inner.read_buf.drain(&mut hashes);
        self.inner.write_buf.drain(&mut write_ops);

        // Advance timer wheel, collect naturally-expired keys.
        let mut timer_keys: Vec<K> = Vec::new();
        if !self.inner.expiry.is_none() {
            let now = self.now_nanos();
            timer_keys = self.inner.timer.lock().advance(now);
        }

        if hashes.is_empty() && write_ops.is_empty() && timer_keys.is_empty() {
            return;
        }

        // Apply everything to the policy under a single lock acquisition.
        let mut capacity_evicted: Vec<K> = Vec::new();
        {
            let mut policy = self.inner.policy.lock();
            let mut timer = self.inner.timer.lock();

            for h in hashes {
                policy.record_access_hash(h);
            }

            for op in write_ops {
                match op {
                    WriteOp::Add {
                        key,
                        weight,
                        expires_at,
                    } => {
                        if let Some(exp) = expires_at {
                            timer.schedule(key.clone(), self.instant_to_nanos(exp));
                        }
                        capacity_evicted.extend(policy.on_insert(key, weight));
                    }
                    WriteOp::Update {
                        key,
                        old_weight,
                        new_weight,
                        expires_at,
                    } => {
                        if let Some(exp) = expires_at {
                            timer.schedule(key.clone(), self.instant_to_nanos(exp));
                        }
                        capacity_evicted.extend(policy.on_update(&key, old_weight, new_weight));
                    }
                    WriteOp::Remove { key } => {
                        timer.cancel(&key);
                        policy.on_remove(&key);
                    }
                    WriteOp::Reschedule { key, expires_at } => {
                        // Store deadline was already updated inline in `get`.
                        timer.schedule(key, self.instant_to_nanos(expires_at));
                    }
                }
            }

            // Remove timer-expired keys from policy tracking.
            for key in &timer_keys {
                policy.on_remove(key);
            }
        }

        // Fire listener for timer-expired entries and evict from store.
        for key in timer_keys {
            if let Some(value) = self.inner.store.remove(&key) {
                if let Some(listener) = &self.inner.listener {
                    listener.on_evict(&key, value, EvictionCause::Expired);
                }
                self.inner.metrics.record_eviction(1);
            }
        }

        // Fire listener and evict capacity-evicted entries.
        self.dispatch_evictions(capacity_evicted, EvictionCause::Capacity);
    }

    fn apply_write_op_sync(&self, op: WriteOp<K>) -> Vec<K> {
        let mut pending: Vec<WriteOp<K>> = Vec::new();
        self.inner.write_buf.drain(&mut pending);
        pending.push(op);

        let mut hashes: Vec<u64> = Vec::new();
        self.inner.read_buf.drain(&mut hashes);

        let mut evicted = Vec::new();
        let mut policy = self.inner.policy.lock();
        let mut timer = self.inner.timer.lock();

        for h in hashes {
            policy.record_access_hash(h);
        }
        for wo in pending {
            match wo {
                WriteOp::Add {
                    key,
                    weight,
                    expires_at,
                } => {
                    if let Some(exp) = expires_at {
                        timer.schedule(key.clone(), self.instant_to_nanos(exp));
                    }
                    evicted.extend(policy.on_insert(key, weight));
                }
                WriteOp::Update {
                    key,
                    old_weight,
                    new_weight,
                    expires_at,
                } => {
                    if let Some(exp) = expires_at {
                        timer.schedule(key.clone(), self.instant_to_nanos(exp));
                    }
                    evicted.extend(policy.on_update(&key, old_weight, new_weight));
                }
                WriteOp::Remove { key } => {
                    timer.cancel(&key);
                    policy.on_remove(&key);
                }
                WriteOp::Reschedule { key, expires_at } => {
                    timer.schedule(key, self.instant_to_nanos(expires_at));
                }
            }
        }
        evicted
    }

    /// Removes capacity-evicted keys from the store, fires listener, records metric.
    fn dispatch_evictions(&self, keys: Vec<K>, cause: EvictionCause) {
        for key in keys {
            if let Some(value) = self.inner.store.remove(&key) {
                if let Some(listener) = &self.inner.listener {
                    listener.on_evict(&key, value, cause);
                }
                self.inner.metrics.record_eviction(1);
            }
        }
    }

    // -----------------------------------------------------------------------
    // Bulk / admin operations
    // -----------------------------------------------------------------------

    /// Removes all entries.
    pub fn invalidate_all(&self) {
        // Collect all entries before clearing, so the listener can be called.
        if self.inner.listener.is_some() {
            // We need the values to fire the listener; collect key-by-key.
            // For invalidate_all we accept the O(n) cost.
            let keys: Vec<K> = {
                let mut ks = Vec::new();
                for shard in self.inner.store.shards() {
                    for key in shard.map.read().keys() {
                        ks.push(key.clone());
                    }
                }
                ks
            };
            for key in keys {
                if let Some(value) = self.inner.store.remove(&key) {
                    self.inner.listener.as_ref().unwrap().on_evict(
                        &key,
                        value,
                        EvictionCause::Explicit,
                    );
                }
            }
        } else {
            self.inner.store.clear();
        }

        let max = self.inner.policy.lock().max_weight();
        let hasher = self.inner.build_hasher.clone();
        *self.inner.policy.lock() = WTinyLfuPolicy::new_with_hasher(max, hasher);
        *self.inner.timer.lock() = TimerWheel::new(self.now_nanos());
    }

    // -----------------------------------------------------------------------
    // Introspection
    // -----------------------------------------------------------------------

    pub fn stats(&self) -> Metrics {
        self.inner.metrics.snapshot()
    }

    pub fn entry_count(&self) -> usize {
        self.inner.store.len()
    }

    pub fn is_empty(&self) -> bool {
        self.inner.store.is_empty()
    }

    pub fn contains(&self, key: &K) -> bool {
        self.inner.store.contains(key)
    }
}
