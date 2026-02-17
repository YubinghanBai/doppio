//! Striped lossy read buffer for frequency-sketch updates.
//!
//! Cache hits are recorded by offering the key hash to this buffer — a single
//! atomic increment plus one atomic store, with **no mutex acquisition**.
//! The buffer is drained by the maintenance thread into the policy's
//! doorkeeper + frequency sketch.
//!
//! ## Design
//!
//! The buffer has `NUM_STRIPES` independent ring-buffer stripes.  Each calling
//! thread is permanently assigned one stripe via a thread-local index so
//! threads avoid colliding on the same stripe's atomic counter.
//!
//! Each stripe holds `STRIPE_CAPACITY` `AtomicU64` hash slots.  When a stripe
//! is full, new offers are **silently dropped** — the frequency sketch is
//! approximate anyway, and losing the occasional read is better than stalling
//! on a lock.
//!
//! ## Known race
//!
//! A writer increments the slot counter atomically but stores the hash value
//! in a separate operation.  A concurrent drain may read a slot before the
//! writer has finished storing, yielding 0 (the sentinel for "empty").  Such
//! reads are skipped; the entry is lost.  This race is intentional and bounded
//! to at most one slot per drain call per stripe.

use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

/// Number of independent stripes.  Must be a power of two.
const NUM_STRIPES: usize = 4;
const STRIPE_MASK: usize = NUM_STRIPES - 1;

/// Capacity of each stripe's ring buffer.  Must be a power of two.
const STRIPE_CAPACITY: usize = 16;

/// Global counter used to assign a stable stripe to each thread.
static STRIPE_COUNTER: AtomicUsize = AtomicUsize::new(0);

thread_local! {
    /// The stripe index for the current thread.  Assigned once on first use.
    static THREAD_STRIPE: usize =
        STRIPE_COUNTER.fetch_add(1, Ordering::Relaxed) & STRIPE_MASK;
}

// ---------------------------------------------------------------------------
// Stripe
// ---------------------------------------------------------------------------

/// One fixed-capacity ring-buffer stripe.
///
/// Padded to 64 bytes to avoid false-sharing with other stripes.
#[repr(align(64))]
struct Stripe {
    /// Hash slots.  0 is the sentinel for "not yet written / empty".
    buf: [AtomicU64; STRIPE_CAPACITY],
    /// Number of claims made.  May exceed `STRIPE_CAPACITY` when full.
    count: AtomicUsize,
}

impl Stripe {
    const fn new() -> Self {
        Stripe {
            buf: [
                AtomicU64::new(0), AtomicU64::new(0),
                AtomicU64::new(0), AtomicU64::new(0),
                AtomicU64::new(0), AtomicU64::new(0),
                AtomicU64::new(0), AtomicU64::new(0),
                AtomicU64::new(0), AtomicU64::new(0),
                AtomicU64::new(0), AtomicU64::new(0),
                AtomicU64::new(0), AtomicU64::new(0),
                AtomicU64::new(0), AtomicU64::new(0),
            ],
            count: AtomicUsize::new(0),
        }
    }

    /// Tries to record `h` in this stripe.
    ///
    /// Returns `false` when the stripe is full.
    #[inline]
    fn offer(&self, h: u64) -> bool {
        let i = self.count.fetch_add(1, Ordering::Relaxed);
        if i < STRIPE_CAPACITY {
            // Use Release ordering so the drain thread's Acquire load sees
            // the stored value if it reads this slot.
            self.buf[i].store(h, Ordering::Release);
            true
        } else {
            false // full — drop silently
        }
    }

    /// Drains all recorded hashes into `out` and resets this stripe.
    ///
    /// Must be called by **one thread at a time** (the maintenance thread).
    fn drain(&self, out: &mut Vec<u64>) {
        // AcqRel: establishes a happens-before with the Release stores in
        // `offer`, so that slots written before the count was read are visible.
        let n = self.count.swap(0, Ordering::AcqRel).min(STRIPE_CAPACITY);
        for i in 0..n {
            // swap(0) clears the slot atomically so a late writer whose store
            // lands *after* this point will be picked up on the next drain
            // (the slot will contain a non-zero value for the next cycle).
            let h = self.buf[i].swap(0, Ordering::AcqRel);
            if h != 0 {
                out.push(h);
            }
        }
    }

    /// Returns `true` when the stripe has hit its capacity.
    #[inline]
    fn is_full(&self) -> bool {
        self.count.load(Ordering::Relaxed) >= STRIPE_CAPACITY
    }
}

// ---------------------------------------------------------------------------
// StripedReadBuffer
// ---------------------------------------------------------------------------

/// A lock-free, lossy, striped read buffer.
///
/// Used to record cache-hit hashes without touching the policy mutex on the
/// hot read path.
pub struct StripedReadBuffer {
    stripes: Box<[Stripe; NUM_STRIPES]>,
}

// SAFETY: Stripe contains only atomic types; it is safe to share across threads.
unsafe impl Send for StripedReadBuffer {}
unsafe impl Sync for StripedReadBuffer {}

impl StripedReadBuffer {
    /// Creates a new striped read buffer.
    pub fn new() -> Self {
        StripedReadBuffer {
            stripes: Box::new([
                Stripe::new(),
                Stripe::new(),
                Stripe::new(),
                Stripe::new(),
            ]),
        }
    }

    /// Offers a key hash to the buffer.
    ///
    /// This is the hot-path call site: it performs one atomic increment and
    /// one atomic store — **no mutex**.
    ///
    /// Returns `false` if the chosen stripe is full; in that case the caller
    /// should schedule a maintenance pass.
    #[inline]
    pub fn offer(&self, h: u64) -> bool {
        let stripe = THREAD_STRIPE.with(|s| *s);
        self.stripes[stripe].offer(h)
    }

    /// Returns `true` if any stripe has reached its capacity, signalling
    /// that a drain should happen soon.
    #[inline]
    pub fn is_full(&self) -> bool {
        self.stripes.iter().any(Stripe::is_full)
    }

    /// Drains all hashes from every stripe into `out`.
    ///
    /// Must be called by a **single thread** at a time.
    pub fn drain(&self, out: &mut Vec<u64>) {
        for stripe in self.stripes.iter() {
            stripe.drain(out);
        }
    }
}

impl Default for StripedReadBuffer {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn offer_and_drain_round_trip() {
        let buf = StripedReadBuffer::new();
        buf.offer(42);
        buf.offer(99);
        buf.offer(0xDEAD_BEEF);

        let mut out = Vec::new();
        buf.drain(&mut out);
        // Order is not guaranteed; check that values are present.
        assert!(out.contains(&42), "missing 42 after drain");
        assert!(out.contains(&99), "missing 99 after drain");
        assert!(out.contains(&0xDEAD_BEEF), "missing 0xDEAD_BEEF after drain");
    }

    #[test]
    fn drain_clears_buffer() {
        let buf = StripedReadBuffer::new();
        buf.offer(1);
        let mut out = Vec::new();
        buf.drain(&mut out);
        out.clear();
        buf.drain(&mut out); // second drain should yield nothing
        assert!(out.is_empty(), "buffer should be empty after drain");
    }

    #[test]
    fn full_stripe_returns_false() {
        let buf = StripedReadBuffer::new();
        // Force all 16 slots of THREAD_STRIPE to fill.
        let mut accepted = 0usize;
        for i in 0..32u64 {
            if buf.stripes[THREAD_STRIPE.with(|s| *s)].offer(i) {
                accepted += 1;
            }
        }
        assert_eq!(accepted, STRIPE_CAPACITY, "only STRIPE_CAPACITY offers accepted");
    }

    #[test]
    fn concurrent_offers_do_not_panic() {
        use std::sync::Arc;
        let buf = Arc::new(StripedReadBuffer::new());
        let mut handles = Vec::new();
        for t in 0..8u64 {
            let b = Arc::clone(&buf);
            handles.push(std::thread::spawn(move || {
                for j in 0..50u64 {
                    b.offer(t * 1000 + j);
                }
            }));
        }
        for h in handles {
            h.join().unwrap();
        }
        let mut out = Vec::new();
        buf.drain(&mut out);
        // We can't assert exact counts (lossy), but there should be some values.
        assert!(!out.is_empty(), "expected some values after concurrent offers");
    }
}
