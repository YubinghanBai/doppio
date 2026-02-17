//! Hit-rate comparison: Doppio vs Moka vs QuickCache.
//!
//! Uses a Zipf(s=1.0) access trace — the standard academic benchmark for
//! cache admission policies.  The same trace is replayed against each cache
//! so the comparison is perfectly fair.
//!
//! Run with:
//!     cargo run --example hit_rate --release

use doppio::CacheBuilder;
use moka::sync::Cache as MokaCache;
use quick_cache::sync::Cache as QuickCache;
use std::time::{Duration, Instant};

/// Cache capacity (number of unique entries each cache may hold).
const CAP: usize = 10_000;
/// Key universe size.  CAP is 10 % of POOL → moderately hard workload.
const POOL: usize = 100_000;
/// Number of accesses in the trace.
const TRACE: usize = 500_000;

// ---------------------------------------------------------------------------
// Zipf(s=1.0) sampler — no external dependency required.
//
// Inverse-CDF derivation:
//   P(X ≤ k) ≈ ln(k) / ln(N)   for large N
//   ⟹  k = N^u  where u ~ Uniform[0,1]
//
// This gives P(X = k) ∝ 1/k, the classic rank-frequency law.
// ---------------------------------------------------------------------------

struct Xorshift64(u64);

impl Xorshift64 {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }

    /// Returns a uniform float in (0, 1].
    fn uniform(&mut self) -> f64 {
        // Use upper 53 bits for a full-precision f64 mantissa.
        let bits = self.next() >> 11;
        // Map [0, 2^53) → (0, 1] by adding 1 and dividing.
        (bits + 1) as f64 / (1u64 << 53) as f64
    }

    /// Zipf(s=1) sample in [0, pool).
    fn zipf(&mut self, pool: usize) -> usize {
        let u = self.uniform();
        // k = floor(pool^u); shift to 0-based.
        let k = (pool as f64).powf(u) as usize;
        k.saturating_sub(1).min(pool - 1)
    }
}

fn generate_trace(seed: u64, pool: usize, len: usize) -> Vec<usize> {
    let mut rng = Xorshift64(seed);
    (0..len).map(|_| rng.zipf(pool)).collect()
}

// ---------------------------------------------------------------------------
// Per-cache runners
// ---------------------------------------------------------------------------

fn run_doppio(trace: &[usize]) -> (usize, Duration) {
    let cache: doppio::Cache<usize, usize> = CacheBuilder::new(CAP as u64).build();
    let start = Instant::now();
    let mut hits = 0usize;
    for &key in trace {
        if cache.get(&key).is_some() {
            hits += 1;
        } else {
            cache.insert(key, key);
        }
    }
    (hits, start.elapsed())
}

fn run_moka(trace: &[usize]) -> (usize, Duration) {
    let cache: MokaCache<usize, usize> = MokaCache::new(CAP as u64);
    let start = Instant::now();
    let mut hits = 0usize;
    for &key in trace {
        if cache.get(&key).is_some() {
            hits += 1;
        } else {
            cache.insert(key, key);
        }
    }
    (hits, start.elapsed())
}

fn run_quick_cache(trace: &[usize]) -> (usize, Duration) {
    let cache: QuickCache<usize, usize> = QuickCache::new(CAP);
    let start = Instant::now();
    let mut hits = 0usize;
    for &key in trace {
        if cache.get(&key).is_some() {
            hits += 1;
        } else {
            cache.insert(key, key);
        }
    }
    (hits, start.elapsed())
}

// ---------------------------------------------------------------------------
// Main
// ---------------------------------------------------------------------------

fn main() {
    println!("╔══════════════════════════════════════════════════════════════╗");
    println!("║          Doppio Cache — Hit Rate Benchmark                   ║");
    println!("╚══════════════════════════════════════════════════════════════╝");
    println!();
    println!("  Distribution : Zipf(s = 1.0)");
    println!("  Key universe : {POOL:>10} unique keys");
    println!("  Capacity     : {CAP:>10} entries  ({:.0}% of universe)",
        CAP as f64 / POOL as f64 * 100.0);
    println!("  Trace length : {TRACE:>10} accesses");
    println!();
    println!("Generating trace…");
    let trace = generate_trace(0xDEAD_BEEF_1234_5678, POOL, TRACE);

    println!("Running benchmarks (cold-start, no warm-up phase)…");
    println!();

    let col_cache = 14usize;
    let col_hits  = 10usize;
    let col_rate  = 10usize;
    let col_time  = 12usize;

    println!(
        "{:<col_cache$} {:>col_hits$} {:>col_rate$} {:>col_time$}",
        "Cache", "Hits", "Hit Rate", "Time (ms)"
    );
    println!("{}", "─".repeat(col_cache + col_hits + col_rate + col_time + 3));

    let print_row = |name: &str, hits: usize, elapsed: Duration| {
        println!(
            "{:<col_cache$} {:>col_hits$} {:>9.2}% {:>col_time$.1}",
            name,
            hits,
            hits as f64 / TRACE as f64 * 100.0,
            elapsed.as_millis(),
        );
    };

    let (hits, elapsed) = run_doppio(&trace);
    print_row("Doppio", hits, elapsed);

    let (hits, elapsed) = run_moka(&trace);
    print_row("Moka", hits, elapsed);

    let (hits, elapsed) = run_quick_cache(&trace);
    print_row("QuickCache", hits, elapsed);

    println!();
    println!("Notes:");
    println!("  • Hit rate is measured in 'online' mode: cache starts cold,");
    println!("    misses trigger an insert, and hits are counted from the start.");
    println!("  • All caches use capacity = {CAP}.");
    println!("  • Time includes miss-path inserts, so lower = better all-round.");
    println!("  • Doppio and Moka both implement W-TinyLFU; QuickCache uses S3-FIFO.");
}
