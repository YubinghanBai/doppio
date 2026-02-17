/// 4-bit approximate frequency counter using Count-Min Sketch.
///
/// Each `u64` in `table` stores **16 saturating 4-bit counters** (nibbles).
/// Counter values are in `[0, 15]`.  The table length is a power of two.
///
/// For each hash `h`, four independent (index, nibble-offset) pairs are
/// computed — one per "depth".  `frequency` returns the **minimum** of the
/// four counter values (the Count-Min estimator).  `increment` adds one to
/// all four counters (saturating at 15).
///
/// **Aging / decay**: when the total number of increments reaches
/// `size * 10`, every counter is **right-shifted by 1** (halved) and the
/// addition counter is halved as well.  This makes the sketch forget old
/// history and adapt to changing hot-sets — the same mechanism used by
/// Caffeine and described in the W-TinyLFU paper (Einziger et al., 2017).
pub struct FrequencySketch {
    table: Vec<u64>,
    /// `table.len()`, always a power of two.
    size: usize,
    /// Total increments since the last reset.  Exposed so the policy can
    /// detect when a reset occurred (to flush the doorkeeper in sync).
    pub additions: u64,
}

/// Four multiplicative hash seeds — chosen to maximise avalanche and
/// minimise inter-depth correlation.
const SEEDS: [u64; 4] = [
    0xABC9_DEFD_ABCD_EF01,
    0xFEDC_BA98_7654_3210,
    0x0F1E_2D3C_4B5A_6978,
    0x9876_5432_10AB_CDEF,
];

/// Bit mask used during reset to avoid carry propagation between nibbles
/// when right-shifting by 1.  Each nibble contributes its high 3 bits only
/// (0111 pattern repeated).
const RESET_MASK: u64 = 0x7777_7777_7777_7777;

impl FrequencySketch {
    /// Creates a new sketch sized for approximately `capacity` distinct items.
    pub fn new(capacity: usize) -> Self {
        // Round up to a power of two; floor at 8 to keep the table meaningful.
        let size = capacity.next_power_of_two().max(8);
        FrequencySketch {
            table: vec![0u64; size],
            size,
            additions: 0,
        }
    }

    /// Estimated frequency of `h`.  Result is in `[0, 15]`.
    ///
    /// Uses Count-Min: the minimum of four independent counter reads.
    #[inline]
    pub fn frequency(&self, h: u64) -> u8 {
        let mut freq = 0x0Fu8;
        for depth in 0..4 {
            let (idx, off) = self.slot(h, depth);
            let count = ((self.table[idx] >> off) & 0xF) as u8;
            freq = freq.min(count);
        }
        freq
    }

    /// Increments the four counters for `h`.
    ///
    /// If the addition threshold is reached a **reset** (halving) is
    /// triggered automatically.
    #[inline]
    pub fn increment(&mut self, h: u64) {
        let mut added = false;
        for depth in 0..4 {
            let (idx, off) = self.slot(h, depth);
            if (self.table[idx] >> off) & 0xF < 15 {
                self.table[idx] += 1u64 << off;
                added = true;
            }
        }
        if added {
            self.additions += 1;
            if self.additions >= self.reset_threshold() {
                self.reset();
            }
        }
    }

    /// Halves all counter values ("aging") and resets the addition counter.
    ///
    /// Called automatically from `increment`; also callable externally when
    /// the doorkeeper needs to be synchronised.
    pub fn reset(&mut self) {
        for cell in &mut self.table {
            // Right-shift each 4-bit nibble by 1.  RESET_MASK zeroes the
            // highest bit of every nibble *before* the shift so that no bit
            // bleeds into the neighbouring nibble.
            *cell = (*cell >> 1) & RESET_MASK;
        }
        self.additions /= 2;
    }

    /// Number of increments that triggers a decay pass.
    #[inline]
    fn reset_threshold(&self) -> u64 {
        self.size as u64 * 10
    }

    /// Returns `(table_index, nibble_bit_offset)` for hash `h` at depth `d`.
    ///
    /// - **Table index**: high 32 bits of the mixed hash, masked to `[0, size)`.
    /// - **Nibble offset**: bits `[28..32)` of the mixed hash select one of
    ///   16 nibbles; the result is multiplied by 4 to obtain a bit offset in
    ///   `{0, 4, 8, …, 60}`.
    ///
    /// Using different seeds per depth produces four nearly-independent hash
    /// functions without additional computation.
    #[inline]
    fn slot(&self, h: u64, depth: usize) -> (usize, usize) {
        let mixed = h.wrapping_mul(SEEDS[depth]);
        let index = ((mixed >> 32) as usize) & (self.size - 1);
        let offset = ((mixed >> 28) as usize & 0xF) << 2; // 0, 4, 8, … 60
        (index, offset)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frequency_zero_for_unseen_key() {
        let sketch = FrequencySketch::new(64);
        assert_eq!(sketch.frequency(0xDEAD_BEEF), 0);
    }

    #[test]
    fn single_increment_gives_one() {
        let mut sketch = FrequencySketch::new(64);
        sketch.increment(42);
        assert_eq!(sketch.frequency(42), 1);
    }

    #[test]
    fn multiple_increments_accumulate() {
        let mut sketch = FrequencySketch::new(64);
        for _ in 0..7 {
            sketch.increment(99);
        }
        assert_eq!(sketch.frequency(99), 7);
    }

    #[test]
    fn saturates_at_15() {
        let mut sketch = FrequencySketch::new(64);
        for _ in 0..25 {
            sketch.increment(1);
        }
        assert_eq!(sketch.frequency(1), 15, "counter must saturate at 15");
    }

    #[test]
    fn independent_keys_do_not_interfere() {
        let mut sketch = FrequencySketch::new(128);
        for _ in 0..5 {
            sketch.increment(1);
        }
        for _ in 0..3 {
            sketch.increment(2);
        }
        // Count-Min over-estimates but never under-estimates.
        assert!(sketch.frequency(1) >= 5);
        assert!(sketch.frequency(2) >= 3);
    }

    #[test]
    fn reset_halves_counters() {
        let mut sketch = FrequencySketch::new(32);
        for _ in 0..10 {
            sketch.increment(7);
        }
        let before = sketch.frequency(7);
        sketch.reset();
        let after = sketch.frequency(7);
        // After halving, value should be roughly before/2 (±1 for rounding).
        assert!(
            after <= before / 2 + 1,
            "after={} expected ≤ {}",
            after,
            before / 2 + 1
        );
    }

    #[test]
    fn additions_trigger_automatic_reset() {
        // size=8 → threshold = 80.
        // Build some frequency for key 42, then drive additions over the
        // threshold using 80 distinct keys.  The reset should halve key 42's
        // counter, leaving it below its pre-reset value.
        let mut sketch = FrequencySketch::new(8);
        for _ in 0..10 {
            sketch.increment(42);
        }
        let before = sketch.frequency(42); // should be 10
        // 80 distinct keys → additions crosses 80+10=90 → reset fires.
        for i in 1_000u64..1_080 {
            sketch.increment(i);
        }
        let after = sketch.frequency(42);
        assert!(
            after < before,
            "reset must halve counters: before={} after={}",
            before,
            after
        );
    }
}
