use doppio::listener::EvictionCause;
use doppio::CacheBuilder;
use std::sync::{Arc, Mutex};
use std::time::Duration;

fn make_cache(cap: u64) -> doppio::Cache<String, String> {
    CacheBuilder::new(cap).build()
}

// ---------------------------------------------------------------------------
// Fundamental API correctness
// ---------------------------------------------------------------------------

#[test]
fn get_returns_none_on_miss() {
    let cache = make_cache(10);
    assert_eq!(cache.get(&"missing".to_string()), None);
}

#[test]
fn insert_and_get() {
    let cache = make_cache(10);
    cache.insert("hello".to_string(), "world".to_string());
    assert_eq!(
        cache.get(&"hello".to_string()),
        Some(Arc::new("world".to_string()))
    );
}

#[test]
fn update_replaces_value() {
    let cache = make_cache(10);
    cache.insert("k".to_string(), "v1".to_string());
    cache.insert("k".to_string(), "v2".to_string());
    assert_eq!(
        cache.get(&"k".to_string()),
        Some(Arc::new("v2".to_string()))
    );
    assert_eq!(cache.entry_count(), 1, "update must not create a second entry");
}

#[test]
fn invalidate_removes_entry() {
    let cache = make_cache(10);
    cache.insert("key".to_string(), "val".to_string());
    cache.invalidate(&"key".to_string());
    assert_eq!(cache.get(&"key".to_string()), None);
}

#[test]
fn stats_tracks_hits_and_misses() {
    let cache = make_cache(10);
    cache.insert("k".to_string(), "v".to_string());
    cache.get(&"k".to_string()); // hit
    cache.get(&"k".to_string()); // hit
    cache.get(&"nope".to_string()); // miss

    let stats = cache.stats();
    assert_eq!(stats.hits, 2);
    assert_eq!(stats.misses, 1);
    assert!(
        (stats.hit_rate - 2.0 / 3.0).abs() < 1e-9,
        "hit_rate = {}",
        stats.hit_rate
    );
}

#[test]
fn cache_is_clone_and_shared() {
    let c1 = make_cache(10);
    let c2 = c1.clone();
    c1.insert("shared".to_string(), "yes".to_string());
    assert!(
        c2.get(&"shared".to_string()).is_some(),
        "cloned handle must see the same entries"
    );
}

// ---------------------------------------------------------------------------
// Capacity enforcement
// ---------------------------------------------------------------------------

#[test]
fn capacity_is_respected_under_load() {
    let cap = 50u64;
    let cache = make_cache(cap);
    // Insert 5× capacity items.
    for i in 0..250u64 {
        cache.insert(i.to_string(), i.to_string());
    }
    assert!(
        cache.entry_count() as u64 <= cap,
        "entry_count {} exceeds capacity {}",
        cache.entry_count(),
        cap
    );
}

// ---------------------------------------------------------------------------
// W-TinyLFU admission semantics
// ---------------------------------------------------------------------------

#[test]
fn hot_items_survive_scan_pollution() {
    let cache: doppio::Cache<u64, u64> = CacheBuilder::new(100).build();

    // Warm up 20 hot keys.
    for i in 0..20u64 {
        cache.insert(i, i);
    }
    // Build frequency (must cross doorkeeper threshold).
    for _ in 0..6 {
        for i in 0..20u64 {
            cache.get(&i);
        }
    }

    // Scan: 400 cold one-hit-wonder insertions.
    for i in 10_000..10_400u64 {
        cache.insert(i, i);
    }

    let survivors: usize = (0..20).filter(|i| cache.get(i).is_some()).count();
    assert!(
        survivors >= 12,
        "only {}/20 hot items survived — W-TinyLFU should do better",
        survivors
    );
}

#[test]
fn high_frequency_key_survives_eviction_pressure() {
    let cache: doppio::Cache<u64, u64> = CacheBuilder::new(10).build();

    for i in 0..10u64 {
        cache.insert(i, i);
    }
    // Heavily access key 0.
    for _ in 0..10 {
        cache.get(&0u64);
    }
    // Insert a wave of new items.
    for i in 100..120u64 {
        cache.insert(i, i);
    }

    assert!(
        cache.get(&0u64).is_some(),
        "key 0 with high frequency should survive"
    );
    assert!(cache.entry_count() as u64 <= 10);
}

// ---------------------------------------------------------------------------
// Concurrency
// ---------------------------------------------------------------------------

#[test]
fn concurrent_insert_and_get() {
    let cache: Arc<doppio::Cache<String, String>> = Arc::new(CacheBuilder::new(1_000).build());
    let mut handles = Vec::new();

    for t in 0..8 {
        let c = Arc::clone(&cache);
        handles.push(std::thread::spawn(move || {
            for j in 0..200 {
                let key = format!("t{}-k{}", t, j);
                c.insert(key.clone(), key.clone());
                let _ = c.get(&key);
            }
        }));
    }
    for h in handles {
        h.join().unwrap();
    }

    assert!(
        cache.entry_count() <= 1_000,
        "entry_count {} exceeds capacity",
        cache.entry_count()
    );
}

// ---------------------------------------------------------------------------
// Weigher
// ---------------------------------------------------------------------------

#[test]
fn weigher_controls_capacity_in_bytes() {
    // Capacity = 100 bytes.  Each value is a Vec<u8> whose weight = len + 1.
    let cache: doppio::Cache<u64, Vec<u8>> = CacheBuilder::new(100)
        .weigher(|_k: &u64, v: &Vec<u8>| v.len() as u64 + 1)
        .build();

    // Insert 20 items of 10 bytes each → weight 11 each.
    // 20 × 11 = 220 > 100, so the cache must evict to stay within budget.
    for i in 0..20u64 {
        cache.insert(i, vec![0u8; 10]);
    }
    // The policy weight must not exceed capacity.
    assert!(
        cache.entry_count() <= 10,
        "too many entries for byte budget: {}",
        cache.entry_count()
    );
}

// ---------------------------------------------------------------------------
// TTL
// ---------------------------------------------------------------------------

#[test]
fn ttl_entry_not_returned_after_expiry() {
    let cache: doppio::Cache<String, String> = CacheBuilder::new(100)
        .time_to_live(Duration::from_millis(50))
        .build();

    cache.insert("k".to_string(), "v".to_string());
    // Before TTL: should be present.
    assert!(cache.get(&"k".to_string()).is_some(), "entry should be alive");

    std::thread::sleep(Duration::from_millis(100));

    // After TTL: must be absent.
    assert!(
        cache.get(&"k".to_string()).is_none(),
        "entry should have expired"
    );
}

#[test]
fn ttl_entry_replaced_resets_expiry() {
    let cache: doppio::Cache<String, String> = CacheBuilder::new(100)
        .time_to_live(Duration::from_millis(80))
        .build();

    cache.insert("k".to_string(), "v1".to_string());
    std::thread::sleep(Duration::from_millis(50));
    // Re-insert resets TTL.
    cache.insert("k".to_string(), "v2".to_string());
    std::thread::sleep(Duration::from_millis(50));
    // 50 + 50 = 100 ms total since first insert, but only 50 ms since replace.
    assert!(
        cache.get(&"k".to_string()).is_some(),
        "re-inserted entry should still be alive"
    );
}

// ---------------------------------------------------------------------------
// TTI
// ---------------------------------------------------------------------------

#[test]
fn tti_entry_expires_without_access() {
    let cache: doppio::Cache<String, String> = CacheBuilder::new(100)
        .time_to_idle(Duration::from_millis(60))
        .build();

    cache.insert("k".to_string(), "v".to_string());
    // Let it idle past the TTI without any get.
    std::thread::sleep(Duration::from_millis(100));

    assert!(
        cache.get(&"k".to_string()).is_none(),
        "idle entry should have expired"
    );
}

#[test]
fn tti_access_resets_idle_timer() {
    let cache: doppio::Cache<String, String> = CacheBuilder::new(100)
        .time_to_idle(Duration::from_millis(80))
        .build();

    cache.insert("k".to_string(), "v".to_string());

    // Keep it alive with periodic reads.
    for _ in 0..3 {
        std::thread::sleep(Duration::from_millis(50));
        assert!(
            cache.get(&"k".to_string()).is_some(),
            "entry should be alive while being accessed"
        );
    }

    // Now stop accessing and let it expire.
    std::thread::sleep(Duration::from_millis(120));
    assert!(
        cache.get(&"k".to_string()).is_none(),
        "entry should expire after idle period"
    );
}

// ---------------------------------------------------------------------------
// EvictionListener
// ---------------------------------------------------------------------------

#[test]
fn listener_fires_on_capacity_eviction() {
    let log: Arc<Mutex<Vec<(u64, EvictionCause)>>> = Arc::new(Mutex::new(Vec::new()));
    let log2 = Arc::clone(&log);

    let cache: doppio::Cache<u64, u64> = CacheBuilder::new(5)
        .eviction_listener(move |key: &u64, _val, cause| {
            log2.lock().unwrap().push((*key, cause));
        })
        .build();

    for i in 0..20u64 {
        cache.insert(i, i * 10);
    }

    let events = log.lock().unwrap();
    assert!(!events.is_empty(), "expected at least one eviction event");
    assert!(
        events.iter().all(|(_, c)| *c == EvictionCause::Capacity),
        "all events should be Capacity"
    );
}

#[test]
fn listener_fires_on_explicit_invalidate() {
    let log: Arc<Mutex<Vec<(u64, EvictionCause)>>> = Arc::new(Mutex::new(Vec::new()));
    let log2 = Arc::clone(&log);

    let cache: doppio::Cache<u64, u64> = CacheBuilder::new(100)
        .eviction_listener(move |key: &u64, _val, cause| {
            log2.lock().unwrap().push((*key, cause));
        })
        .build();

    cache.insert(42, 420);
    cache.invalidate(&42);

    let events = log.lock().unwrap();
    assert_eq!(events.len(), 1);
    assert_eq!(events[0], (42, EvictionCause::Explicit));
}

#[test]
fn listener_fires_on_ttl_expiry() {
    let log: Arc<Mutex<Vec<EvictionCause>>> = Arc::new(Mutex::new(Vec::new()));
    let log2 = Arc::clone(&log);

    let cache: doppio::Cache<u64, u64> = CacheBuilder::new(100)
        .time_to_live(Duration::from_millis(50))
        .eviction_listener(move |_key, _val, cause| {
            log2.lock().unwrap().push(cause);
        })
        .build();

    cache.insert(1, 100);

    // Wait for TTL to elapse; expiry is detected inline on `get`.
    std::thread::sleep(Duration::from_millis(100));
    let _ = cache.get(&1); // triggers inline expiry + listener

    let events = log.lock().unwrap();
    assert!(
        events.iter().any(|c| *c == EvictionCause::Expired),
        "expected an Expired event"
    );
}
