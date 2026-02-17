pub mod timer_wheel;

use std::time::{Duration, Instant};

/// Determines how long a cache entry lives.
///
/// Returning `None` means the entry does not expire.
/// This trait will be integrated with `TimerWheel` in Phase 3.
pub trait Expiry<K, V>: Send + Sync {
    /// Called when an entry is first inserted.
    fn expire_after_create(&self, key: &K, value: &V, now: Instant) -> Option<Duration>;

    /// Called when an entry is read. Return the new remaining duration,
    /// or the existing duration to leave it unchanged.
    fn expire_after_read(
        &self,
        key: &K,
        value: &V,
        now: Instant,
        current_duration: Duration,
    ) -> Option<Duration> {
        let _ = (key, value, now);
        Some(current_duration)
    }

    /// Called when an entry is updated.
    fn expire_after_update(
        &self,
        key: &K,
        value: &V,
        now: Instant,
        current_duration: Duration,
    ) -> Option<Duration> {
        let _ = (key, value, now);
        Some(current_duration)
    }
}

/// A fixed TTL: every entry expires `ttl` after it was written.
pub struct FixedTtl(pub Duration);

impl<K, V> Expiry<K, V> for FixedTtl {
    fn expire_after_create(&self, _key: &K, _value: &V, _now: Instant) -> Option<Duration> {
        Some(self.0)
    }
}

/// A fixed TTI (time-to-idle): entries expire `tti` after the last access.
pub struct FixedTti(pub Duration);

impl<K, V> Expiry<K, V> for FixedTti {
    fn expire_after_create(&self, _key: &K, _value: &V, _now: Instant) -> Option<Duration> {
        Some(self.0)
    }

    fn expire_after_read(
        &self,
        _key: &K,
        _value: &V,
        _now: Instant,
        _current_duration: Duration,
    ) -> Option<Duration> {
        Some(self.0) // reset the countdown on every read
    }
}
