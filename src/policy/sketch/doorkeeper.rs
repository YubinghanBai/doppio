/// A counting Bloom filter that gates frequency-sketch updates.
///
/// **Role in W-TinyLFU**: before incrementing the frequency sketch for a
/// key, check whether that key has been seen at least once before.  If this
/// is the first sighting, record it in the doorkeeper but *do not* touch the
/// sketch.  Only on the second (and subsequent) access does the sketch get
/// an increment.
///
/// This eliminates ~50 % of all sketch updates caused by one-hit-wonders
/// (items accessed exactly once), keeping the sketch's limited counter bits
/// focused on genuinely popular keys.
///
/// The doorkeeper is **reset together with the sketch** so that its state
/// stays in sync with the frequency estimates.
///
/// Implementation: a standard k-hash Bloom filter backed by a bit vector of
/// 64-bit words.  k = 4 hash functions are used, each derived from a
/// different multiplicative seed.
pub struct Doorkeeper {
    bits: Vec<u64>,
    /// Total number of bits; always a power of two.
    m: usize,
    /// Number of distinct keys inserted since the last `clear`.
    pub additions: usize,
}

/// Four independent multiplicative hash seeds for the Bloom filter.
const SEEDS: [u64; 4] = [
    0xF135_7AEA_2E62_A9C5,
    0x0A3F_29B9_C7E4_A1D3,
    0x9C3D_2F1A_5B7E_4C8D,
    0x2B5E_7A1C_4F9D_3E6B,
];

impl Doorkeeper {
    /// Creates a new doorkeeper sized for `expected_items` distinct keys at
    /// roughly 1 % false-positive rate (≈ 10 bits per item, k = 4).
    pub fn new(expected_items: usize) -> Self {
        let num_bits = (expected_items * 10).next_power_of_two().max(64);
        let num_words = num_bits / 64;
        Doorkeeper {
            bits: vec![0u64; num_words],
            m: num_bits,
            additions: 0,
        }
    }

    /// Returns `true` if `h` is (probably) already recorded in the filter.
    ///
    /// False positives are possible; false negatives are not.
    #[inline]
    pub fn contains(&self, h: u64) -> bool {
        SEEDS.iter().all(|&seed| {
            let bit = self.bit_index(h, seed);
            (self.bits[bit >> 6] >> (bit & 63)) & 1 == 1
        })
    }

    /// Records `h` in the filter.
    ///
    /// Returns `true` if `h` was **already present** — meaning the sketch
    /// should now be incremented for this key.  Returns `false` if this is
    /// the first sighting — only the doorkeeper is updated.
    #[inline]
    pub fn insert(&mut self, h: u64) -> bool {
        let already = self.contains(h);
        if !already {
            for &seed in &SEEDS {
                let bit = self.bit_index(h, seed);
                self.bits[bit >> 6] |= 1u64 << (bit & 63);
            }
            self.additions += 1;
        }
        already
    }

    /// Resets the doorkeeper to an empty state.
    ///
    /// Must be called whenever the frequency sketch is reset so that the
    /// "first seen" information stays consistent with the halved counters.
    pub fn clear(&mut self) {
        self.bits.fill(0);
        self.additions = 0;
    }

    /// Maps `(hash, seed)` to a bit position in `[0, m)`.
    #[inline]
    fn bit_index(&self, h: u64, seed: u64) -> usize {
        (h.wrapping_mul(seed) >> 32) as usize & (self.m - 1)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn first_insert_returns_false() {
        let mut dk = Doorkeeper::new(128);
        assert!(!dk.insert(42), "first sighting must return false");
    }

    #[test]
    fn second_insert_returns_true() {
        let mut dk = Doorkeeper::new(128);
        dk.insert(42);
        assert!(dk.insert(42), "second sighting must return true");
    }

    #[test]
    fn contains_false_before_any_insert() {
        let dk = Doorkeeper::new(128);
        assert!(!dk.contains(0xCAFE));
    }

    #[test]
    fn clear_resets_all_bits() {
        let mut dk = Doorkeeper::new(128);
        for i in 0..50u64 {
            dk.insert(i);
        }
        dk.clear();
        for i in 0..50u64 {
            assert!(!dk.contains(i), "key {} should be gone after clear", i);
        }
        assert_eq!(dk.additions, 0);
    }

    #[test]
    fn false_positive_rate_is_low() {
        // Insert 100 keys, then check 10 000 non-inserted keys.
        // With 10 bits/item and k=4 the theoretical FPR is ~0.82 %.
        let mut dk = Doorkeeper::new(100);
        for i in 0..100u64 {
            dk.insert(i);
        }
        let fp_count = (1_000..11_000u64).filter(|&h| dk.contains(h)).count();
        // Allow up to 5× theoretical rate as a loose bound.
        assert!(
            fp_count < 500,
            "false positive count {} is too high (>5 % of 10 000)",
            fp_count
        );
    }
}
