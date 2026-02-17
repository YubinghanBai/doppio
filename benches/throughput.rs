//! Throughput benchmarks: Doppio vs Moka vs QuickCache.
//!
//! Each group benchmarks the same workload across all three caches so
//! criterion can generate side-by-side HTML reports.
//!
//! Run with:
//!     cargo bench --bench throughput

use criterion::{black_box, criterion_group, criterion_main, BenchmarkId, Criterion, Throughput};
use doppio::CacheBuilder;
use moka::sync::Cache as MokaCache;
use quick_cache::sync::Cache as QuickCache;

/// Number of entries each cache is pre-filled with and its logical capacity.
const CAP: u64 = 10_000;

/// Operations executed per criterion iteration (hot-loop size).
const OPS: u64 = 1_000;

// ---------------------------------------------------------------------------
// Group 1: get_hit
// ---------------------------------------------------------------------------
// All keys are present → measures pure read throughput with no eviction.

fn bench_get_hit(c: &mut Criterion) {
    let doppio: doppio::Cache<u64, u64> = CacheBuilder::new(CAP).build();
    for i in 0..CAP {
        doppio.insert(i, i * 2);
    }

    let moka: MokaCache<u64, u64> = MokaCache::new(CAP);
    for i in 0..CAP {
        moka.insert(i, i * 2);
    }

    let qc: QuickCache<u64, u64> = QuickCache::new(CAP as usize);
    for i in 0..CAP {
        qc.insert(i, i * 2);
    }

    let mut group = c.benchmark_group("get_hit");
    group.throughput(Throughput::Elements(OPS));

    group.bench_function("doppio", |b| {
        b.iter(|| {
            for i in 0..OPS {
                black_box(doppio.get(black_box(&i)));
            }
        })
    });

    group.bench_function("moka", |b| {
        b.iter(|| {
            for i in 0..OPS {
                black_box(moka.get(black_box(&i)));
            }
        })
    });

    group.bench_function("quick_cache", |b| {
        b.iter(|| {
            for i in 0..OPS {
                black_box(qc.get(black_box(&i)));
            }
        })
    });

    group.finish();
}

// ---------------------------------------------------------------------------
// Group 2: insert_evicting
// ---------------------------------------------------------------------------
// Sequential inserts of always-new keys — cache must evict to stay within
// capacity on every batch.

fn bench_insert_evicting(c: &mut Criterion) {
    let mut group = c.benchmark_group("insert_evicting");
    group.throughput(Throughput::Elements(OPS));

    group.bench_function("doppio", |b| {
        let cache: doppio::Cache<u64, u64> = CacheBuilder::new(CAP).build();
        let mut key = 0u64;
        b.iter(|| {
            for _ in 0..OPS {
                cache.insert(black_box(key), black_box(key));
                key = key.wrapping_add(1);
            }
        })
    });

    group.bench_function("moka", |b| {
        let cache: MokaCache<u64, u64> = MokaCache::new(CAP);
        let mut key = 0u64;
        b.iter(|| {
            for _ in 0..OPS {
                cache.insert(black_box(key), black_box(key));
                key = key.wrapping_add(1);
            }
        })
    });

    group.bench_function("quick_cache", |b| {
        let cache: QuickCache<u64, u64> = QuickCache::new(CAP as usize);
        let mut key = 0u64;
        b.iter(|| {
            for _ in 0..OPS {
                cache.insert(black_box(key), black_box(key));
                key = key.wrapping_add(1);
            }
        })
    });

    group.finish();
}

// ---------------------------------------------------------------------------
// Group 3: mixed_80r_20w
// ---------------------------------------------------------------------------
// 80 % reads, 20 % writes, working set = 2× capacity (produces eviction).
// Keys cycle with a prime step to vary the access pattern.

fn bench_mixed_80r_20w(c: &mut Criterion) {
    const WORKING_SET: u64 = CAP * 2;
    const STEP: u64 = 7_919; // prime

    let mut group = c.benchmark_group("mixed_80r_20w");
    group.throughput(Throughput::Elements(OPS));

    group.bench_function("doppio", |b| {
        let cache: doppio::Cache<u64, u64> = CacheBuilder::new(CAP).build();
        for i in 0..CAP {
            cache.insert(i, i);
        }
        let mut cursor = 0u64;
        b.iter(|| {
            for i in 0..OPS {
                let k = cursor % WORKING_SET;
                if i % 5 == 0 {
                    cache.insert(black_box(k), black_box(k));
                } else {
                    black_box(cache.get(black_box(&k)));
                }
                cursor = cursor.wrapping_add(STEP);
            }
        })
    });

    group.bench_function("moka", |b| {
        let cache: MokaCache<u64, u64> = MokaCache::new(CAP);
        for i in 0..CAP {
            cache.insert(i, i);
        }
        let mut cursor = 0u64;
        b.iter(|| {
            for i in 0..OPS {
                let k = cursor % WORKING_SET;
                if i % 5 == 0 {
                    cache.insert(black_box(k), black_box(k));
                } else {
                    black_box(cache.get(black_box(&k)));
                }
                cursor = cursor.wrapping_add(STEP);
            }
        })
    });

    group.bench_function("quick_cache", |b| {
        let cache: QuickCache<u64, u64> = QuickCache::new(CAP as usize);
        for i in 0..CAP {
            cache.insert(i, i);
        }
        let mut cursor = 0u64;
        b.iter(|| {
            for i in 0..OPS {
                let k = cursor % WORKING_SET;
                if i % 5 == 0 {
                    cache.insert(black_box(k), black_box(k));
                } else {
                    black_box(cache.get(black_box(&k)));
                }
                cursor = cursor.wrapping_add(STEP);
            }
        })
    });

    group.finish();
}

// ---------------------------------------------------------------------------
// Group 4: concurrent_mixed — 8-thread, 50 % reads / 50 % writes
// ---------------------------------------------------------------------------

fn bench_concurrent_mixed(c: &mut Criterion) {
    use std::sync::{Arc, Barrier};
    use std::time::{Duration, Instant};

    const THREADS: usize = 8;
    const OPS_PER_THREAD: u64 = 2_000;
    const WORKING_SET: u64 = CAP * 2;

    let mut group = c.benchmark_group("concurrent_8t_50r_50w");
    group.throughput(Throughput::Elements(THREADS as u64 * OPS_PER_THREAD));

    // --- Doppio ---
    {
        let cache: doppio::Cache<u64, u64> = CacheBuilder::new(CAP).build();
        for i in 0..CAP {
            cache.insert(i, i);
        }
        let cache = Arc::new(cache);

        group.bench_function("doppio", |b| {
            b.iter_custom(|iters| {
                let mut total = Duration::ZERO;
                for _ in 0..iters {
                    let barrier = Arc::new(Barrier::new(THREADS + 1));
                    let handles: Vec<_> = (0..THREADS)
                        .map(|t| {
                            let c = Arc::clone(&cache);
                            let bar = Arc::clone(&barrier);
                            std::thread::spawn(move || {
                                bar.wait();
                                let start = Instant::now();
                                let base = t as u64 * OPS_PER_THREAD;
                                for j in 0..OPS_PER_THREAD {
                                    let k = (base.wrapping_add(j * 7_919)) % WORKING_SET;
                                    if j % 2 == 0 {
                                        c.insert(black_box(k), black_box(k));
                                    } else {
                                        black_box(c.get(black_box(&k)));
                                    }
                                }
                                start.elapsed()
                            })
                        })
                        .collect();
                    barrier.wait();
                    let elapsed = handles.into_iter().map(|h| h.join().unwrap()).max().unwrap();
                    total += elapsed;
                }
                total
            })
        });
    }

    // --- Moka ---
    {
        let cache: MokaCache<u64, u64> = MokaCache::new(CAP);
        for i in 0..CAP {
            cache.insert(i, i);
        }
        let cache = Arc::new(cache);

        group.bench_function("moka", |b| {
            b.iter_custom(|iters| {
                let mut total = Duration::ZERO;
                for _ in 0..iters {
                    let barrier = Arc::new(Barrier::new(THREADS + 1));
                    let handles: Vec<_> = (0..THREADS)
                        .map(|t| {
                            let c = Arc::clone(&cache);
                            let bar = Arc::clone(&barrier);
                            std::thread::spawn(move || {
                                bar.wait();
                                let start = Instant::now();
                                let base = t as u64 * OPS_PER_THREAD;
                                for j in 0..OPS_PER_THREAD {
                                    let k = (base.wrapping_add(j * 7_919)) % WORKING_SET;
                                    if j % 2 == 0 {
                                        c.insert(black_box(k), black_box(k));
                                    } else {
                                        black_box(c.get(black_box(&k)));
                                    }
                                }
                                start.elapsed()
                            })
                        })
                        .collect();
                    barrier.wait();
                    let elapsed = handles.into_iter().map(|h| h.join().unwrap()).max().unwrap();
                    total += elapsed;
                }
                total
            })
        });
    }

    // --- QuickCache ---
    {
        let cache: QuickCache<u64, u64> = QuickCache::new(CAP as usize);
        for i in 0..CAP {
            cache.insert(i, i);
        }
        let cache = Arc::new(cache);

        group.bench_function("quick_cache", |b| {
            b.iter_custom(|iters| {
                let mut total = Duration::ZERO;
                for _ in 0..iters {
                    let barrier = Arc::new(Barrier::new(THREADS + 1));
                    let handles: Vec<_> = (0..THREADS)
                        .map(|t| {
                            let c = Arc::clone(&cache);
                            let bar = Arc::clone(&barrier);
                            std::thread::spawn(move || {
                                bar.wait();
                                let start = Instant::now();
                                let base = t as u64 * OPS_PER_THREAD;
                                for j in 0..OPS_PER_THREAD {
                                    let k = (base.wrapping_add(j * 7_919)) % WORKING_SET;
                                    if j % 2 == 0 {
                                        c.insert(black_box(k), black_box(k));
                                    } else {
                                        black_box(c.get(black_box(&k)));
                                    }
                                }
                                start.elapsed()
                            })
                        })
                        .collect();
                    barrier.wait();
                    let elapsed = handles.into_iter().map(|h| h.join().unwrap()).max().unwrap();
                    total += elapsed;
                }
                total
            })
        });
    }

    group.finish();
}

criterion_group!(
    benches,
    bench_get_hit,
    bench_insert_evicting,
    bench_mixed_80r_20w,
    bench_concurrent_mixed,
);
criterion_main!(benches);
