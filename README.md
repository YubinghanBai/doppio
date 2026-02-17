# Doppio ☕

**A high-performance, concurrent in-memory cache for Rust.**

Doppio implements the **W-TinyLFU** admission policy — the same algorithm
powering Caffeine (Java's fastest cache) — while keeping the API small and
the lock contention low.

[![Crates.io](https://img.shields.io/crates/v/doppio.svg)](https://crates.io/crates/doppio)
[![Docs.rs](https://img.shields.io/docsrs/doppio)](https://docs.rs/doppio)
[![License](https://img.shields.io/crates/l/doppio)](LICENSE-MIT)
[![GitHub](https://img.shields.io/badge/github-YubinghanBai%2Fdoppio-blue)](https://github.com/YubinghanBai/doppio)

---

## Features

- **W-TinyLFU** — Window LFU + Segmented LRU with a frequency sketch; resists
  scan pollution and offers near-optimal hit rates
- **Lock-free read path** — `get()` never acquires a mutex; frequency updates
  are batched via a striped ring buffer
- **TTL & TTI** — per-cache time-to-live and time-to-idle with inline expiry
  detection (no background scan overhead on `get`)
- **Custom weigher** — bound capacity by bytes, business priority, or any other
  metric instead of entry count
- **Eviction listener** — synchronous callbacks for capacity eviction, expiry,
  and explicit invalidation
- **Clone-able handle** — `Cache<K, V>: Clone` shares the same interior
  instance, making it trivial to pass across threads or tasks
- **No unsafe code**, no async runtime required

---

## Quick Start

```toml
[dependencies]
doppio = "0.1"
```

```rust
use doppio::CacheBuilder;
use std::sync::Arc;

// Entry-count capacity
let cache: doppio::Cache<String, String> = CacheBuilder::new(10_000).build();

cache.insert("hello".to_string(), "world".to_string());
assert_eq!(cache.get(&"hello".to_string()), Some(Arc::new("world".to_string())));

// TTL — entries expire 60 s after insertion
let ttl_cache: doppio::Cache<String, String> = CacheBuilder::new(10_000)
    .time_to_live(std::time::Duration::from_secs(60))
    .build();

// TTI — entries expire 30 s after the last access
let tti_cache: doppio::Cache<String, String> = CacheBuilder::new(10_000)
    .time_to_idle(std::time::Duration::from_secs(30))
    .build();

// Byte-budget weigher — cap at ~50 MB
let byte_cache: doppio::Cache<String, Vec<u8>> = CacheBuilder::new(50 * 1024 * 1024)
    .weigher(|_k: &String, v: &Vec<u8>| v.len() as u64 + 1)
    .build();

// Eviction listener
use doppio::listener::EvictionCause;
let cache: doppio::Cache<u64, String> = CacheBuilder::new(1_000)
    .eviction_listener(|key: &u64, _val, cause| {
        println!("evicted key={key} cause={cause:?}");
    })
    .build();
```

---

## Custom Weigher

Any type that implements `Weigher<K, V>` can be plugged in:

```rust
use doppio::{CacheBuilder, weigher::Weigher};

struct PriorityWeigher;

impl Weigher<u64, MyValue> for PriorityWeigher {
    fn weigh(&self, _key: &u64, val: &MyValue) -> u64 {
        // Higher priority items are cheaper to keep (count as 1),
        // low priority items are expensive (count as their byte size).
        if val.priority > 8 { 1 } else { val.estimated_bytes() as u64 }
    }
}

let cache = CacheBuilder::new(4096)
    .weigher_impl(PriorityWeigher)
    .build();
```

Built-in weigher patterns:

| Pattern | Closure example |
|---------|----------------|
| Entry count (default) | — |
| Byte size | `\|_k, v: &Vec<u8>\| v.len() as u64 + 1` |
| Serialized size | `\|_k, v\| bincode::serialized_size(v).unwrap_or(1)` |
| Business priority | `\|_k, v\| (10 - v.priority as u64).max(1)` |

---

## Architecture

```
┌──────────────────────────────────────────────────────────────────┐
│  Cache<K, V>  (Clone-able Arc handle)                            │
│                                                                  │
│  ┌──────────────────────────────────────────────────────────┐   │
│  │  Inner<K, V>                                             │   │
│  │                                                          │   │
│  │  ┌─────────────────────┐   ┌──────────────────────────┐ │   │
│  │  │  ShardedStore<K,V>  │   │  WTinyLfuPolicy<K>       │ │   │
│  │  │  64 × RwLock shards │   │  (Mutex — maintenance    │ │   │
│  │  │  key → StoreEntry { │   │   only, not on get path) │ │   │
│  │  │    Arc<V>           │   │                          │ │   │
│  │  │    expires_at       │   │  Window LRU  (1 %)       │ │   │
│  │  │  }                  │   │  Probation   (~20%)      │ │   │
│  │  └─────────────────────┘   │  Protected   (~79%)      │ │   │
│  │                            │                          │ │   │
│  │  ┌─────────────────────┐   │  FrequencySketch         │ │   │
│  │  │  StripedReadBuffer  │   │  (4-bit CMS, 4 hashes)   │ │   │
│  │  │  4 × 16 AtomicU64   │   │  Doorkeeper Bloom filter │ │   │
│  │  │  (lock-free offers) │   └──────────────────────────┘ │   │
│  │  └─────────────────────┘                                 │   │
│  │                                                          │   │
│  │  ┌─────────────────────┐   ┌──────────────────────────┐ │   │
│  │  │  WriteBuffer<K>     │   │  TimerWheel<K>           │ │   │
│  │  │  ArrayQueue cap=128 │   │  5 levels, ns resolution │ │   │
│  │  │  (lossless MPSC)    │   │  cascade on advance()    │ │   │
│  │  └─────────────────────┘   └──────────────────────────┘ │   │
│  └──────────────────────────────────────────────────────────┘   │
└──────────────────────────────────────────────────────────────────┘

get() hot path  ─→  shard read-lock (no policy lock)
                    inline expiry check
                    lock-free read-buffer offer
                    [TTI] immediate store expiry update

insert() path   ─→  shard write-lock
                    write-buffer push → try_maintain()
                    [if buffer full] sync drain + policy update
```

### W-TinyLFU in brief

W-TinyLFU divides cache space into a **Window** (1%) and a **Main** segment
(99%, split into Probation and Protected).  New entries enter the Window; on
eviction, the Window victim competes against the Main victim using a 4-bit
count-min sketch.  A Bloom filter (Doorkeeper) gates first appearances to
prevent frequency inflation from scan traffic.

---

## Benchmarks

All benchmarks run on Apple M-series (arm64), single-threaded unless noted.
`cargo bench --bench throughput` and `cargo run --example hit_rate --release`.

### Throughput (ops / second)

Workload: 10 000-entry cache, 1 000 ops per iteration.

| Benchmark | Doppio | Moka | QuickCache |
|-----------|-------:|-----:|-----------:|
| `get_hit` — all hits, hot path | **27.0 M/s** | 12.4 M/s | 130.9 M/s |
| `insert_evicting` — all-new keys | **3.96 M/s** | 1.75 M/s | 23.6 M/s |
| `mixed_80r_20w` — 80 % reads | **12.6 M/s** | 4.3 M/s | 49.6 M/s |
| `concurrent_8t` — 8 threads, 50 % reads | **7.3 M/s** | 2.6 M/s | 30.7 M/s |

**Doppio is ~2–3× faster than Moka** across all workloads.  QuickCache is
faster still because its S3-FIFO algorithm is structurally simpler than
W-TinyLFU, trading some hit-rate accuracy for lower per-op cost.

### Hit Rate (Zipf s = 1.0 distribution)

Setup: 100 000-key universe, 10 000-entry capacity (10 %), 500 000 accesses,
cold-start (no separate warm-up phase).

| Cache | Hits | Hit Rate | Time |
|-------|-----:|--------:|-----:|
| Doppio | 359 791 | 71.96 % | 62 ms |
| Moka | 380 589 | **76.12 %** | 147 ms |
| QuickCache | 381 513 | **76.30 %** | **14 ms** |

> **Note (v0.1):** Doppio's ~4 % hit-rate gap vs Moka stems from the
> current *inline* maintenance model: frequency updates are batched via a
> lossy read buffer and applied only when a thread happens to acquire the
> maintenance lock.  Under write-heavy traces some updates are dropped.
> **Phase 5** (dedicated background maintenance thread) will eliminate this
> gap.  Even today, Doppio's 72 % hit rate far exceeds a plain LRU (~55–60 %
> on the same trace).

### Comparison Summary

| | Doppio | Moka | QuickCache |
|---|---|---|---|
| Algorithm | W-TinyLFU | W-TinyLFU | S3-FIFO |
| Read throughput | ✅ 2× faster than Moka | baseline | ✅ 5× faster |
| Write throughput | ✅ 2× faster than Moka | baseline | ✅ 6× faster |
| Hit rate | ⚠️ ~72 % (v0.1) | ~76 % | ~76 % |
| TTL / TTI | ✅ | ✅ | ✅ |
| Eviction listener | ✅ | ✅ | ❌ |
| Custom weigher | ✅ | ✅ | ❌ |
| No async runtime | ✅ | ✅ | ✅ |
| Codebase size | small | large | small |

---

## Roadmap

- [x] **v0.1** — W-TinyLFU + TTL/TTI + Weigher + EvictionListener
- [ ] **v0.2** — Dedicated background maintenance thread (closes hit-rate gap)
- [ ] **v0.3** — `AsyncCache` behind a `tokio` feature flag
- [ ] **v0.4** — Real-trace benchmarks (LIRS, ARC, UMass traces)

---

## License

Licensed under either of [MIT](LICENSE-MIT) or
[Apache 2.0](LICENSE-APACHE) at your option.
