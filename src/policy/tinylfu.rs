use std::hash::Hash;

use ahash::{AHashMap, RandomState};

use super::sketch::{Doorkeeper, FrequencySketch};
use super::Policy;

// ---------------------------------------------------------------------------
// Sentinel layout
//
// The first six slots of `nodes` are permanent HEAD/TAIL sentinels — one
// pair for each queue.  Real entries start at index 6.  Sentinels always
// have `key = None` and are never evicted or looked up via the index.
// ---------------------------------------------------------------------------
const WINDOW_HEAD: usize = 0;
const WINDOW_TAIL: usize = 1;
const PROBATION_HEAD: usize = 2;
const PROBATION_TAIL: usize = 3;
const PROTECTED_HEAD: usize = 4;
const PROTECTED_TAIL: usize = 5;
const NULL: usize = usize::MAX;
const SENTINEL_COUNT: usize = 6;

/// Which queue a cache entry currently belongs to.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Queue {
    Window,
    Probation,
    Protected,
}

/// A single node in the entry arena.
struct TlfuNode<K> {
    /// `None` only for sentinel slots.
    key: Option<K>,
    /// Precomputed hash of the key — stored so we can compute sketch
    /// frequencies during eviction without rehashing.
    key_hash: u64,
    weight: u64,
    /// Index of the predecessor in the doubly-linked list.
    prev: usize,
    /// Index of the successor in the doubly-linked list.
    next: usize,
    queue: Queue,
}

// ---------------------------------------------------------------------------
// W-TinyLFU Policy
// ---------------------------------------------------------------------------

/// W-TinyLFU eviction policy.
///
/// ## Algorithm
///
/// Capacity is partitioned into three segments:
///
/// | Segment       | Default share    | Role |
/// |---------------|------------------|------|
/// | **Window**    | 1 % of capacity  | Admits every new entry; protects recently arrived items from premature rejection |
/// | **Probation** | ~20 % of main    | Candidates awaiting frequency proof; eviction victim pool |
/// | **Protected** | ~80 % of main    | Items that demonstrated both recency and frequency |
///
/// ### Insertion path
/// 1. New entry is placed at the **head (MRU) of the Window**.
/// 2. When the Window overflows the **LRU Window entry becomes a candidate**.
/// 3. If Main cache has room, the candidate is admitted to **Probation**.
/// 4. If Main cache is full:
///    - **victim** = LRU entry of Probation.
///    - `freq(candidate) > freq(victim)` → admit candidate, evict victim.
///    - Otherwise → reject candidate (evict it immediately).
///
/// ### Read path
/// 1. Frequency sketch is updated via the doorkeeper filter.
/// 2. Probation hit → entry is promoted to **Protected** (MRU).
/// 3. Protected overflow → LRU Protected entry demoted back to Probation.
///
/// ## References
/// - Einziger, Friedman, Manes (2017). *TinyLFU: A Highly Efficient Cache
///   Admission Policy.* ACM Transactions on Storage.
/// - Caffeine source: `com.github.benmanes.caffeine.cache.BoundedLocalCache`
pub struct WTinyLfuPolicy<K> {
    sketch: FrequencySketch,
    doorkeeper: Doorkeeper,
    /// Hasher used for computing `key_hash` on insert / access.
    build_hasher: RandomState,

    /// Central node arena — both sentinels and real entries live here.
    nodes: Vec<TlfuNode<K>>,
    /// Maps `K → arena index` for O(1) access.
    index: AHashMap<K, usize>,
    /// Recycled arena slots.
    free_list: Vec<usize>,

    window_weight: u64,
    probation_weight: u64,
    protected_weight: u64,

    max_total: u64,
    /// ~1 % of `max_total`; minimum 1.
    max_window: u64,
    /// ~80 % of main (= `max_total − max_window`); minimum 1.
    max_protected: u64,
}

impl<K: Hash + Eq + Clone + Send> WTinyLfuPolicy<K> {
    /// Creates a new policy with the given capacity and a fresh random hasher.
    pub fn new(max_capacity: u64) -> Self {
        Self::new_with_hasher(max_capacity, RandomState::new())
    }

    /// Creates a new policy with a caller-supplied hasher.
    ///
    /// Useful when the cache layer needs to hash keys externally (e.g., for
    /// the read buffer) using the *same* hash function as the policy's
    /// frequency sketch and doorkeeper.
    pub fn new_with_hasher(max_capacity: u64, hasher: RandomState) -> Self {
        let max_total = max_capacity.max(1);

        // W-TinyLFU standard ratios (Caffeine defaults):
        //   window    : 1 % of total
        //   main      : 99 % of total
        //   protected : 80 % of main
        //   probation : 20 % of main
        let max_window = (max_total / 100).max(1);
        let max_main = max_total - max_window;
        let max_protected = (max_main * 4 / 5).max(1);

        let cap = max_total as usize;

        // Allocate the six sentinel nodes.
        let mut nodes: Vec<TlfuNode<K>> = Vec::with_capacity(SENTINEL_COUNT + cap);
        let sentinel_queues = [
            Queue::Window,    // 0 = WINDOW_HEAD
            Queue::Window,    // 1 = WINDOW_TAIL
            Queue::Probation, // 2 = PROBATION_HEAD
            Queue::Probation, // 3 = PROBATION_TAIL
            Queue::Protected, // 4 = PROTECTED_HEAD
            Queue::Protected, // 5 = PROTECTED_TAIL
        ];
        for q in sentinel_queues {
            nodes.push(TlfuNode {
                key: None,
                key_hash: 0,
                weight: 0,
                prev: NULL,
                next: NULL,
                queue: q,
            });
        }
        // Wire sentinel pairs: HEAD.next = TAIL, TAIL.prev = HEAD.
        nodes[WINDOW_HEAD].next = WINDOW_TAIL;
        nodes[WINDOW_TAIL].prev = WINDOW_HEAD;
        nodes[PROBATION_HEAD].next = PROBATION_TAIL;
        nodes[PROBATION_TAIL].prev = PROBATION_HEAD;
        nodes[PROTECTED_HEAD].next = PROTECTED_TAIL;
        nodes[PROTECTED_TAIL].prev = PROTECTED_HEAD;

        WTinyLfuPolicy {
            sketch: FrequencySketch::new(cap),
            doorkeeper: Doorkeeper::new(cap),
            build_hasher: hasher,
            nodes,
            index: AHashMap::with_capacity(cap),
            free_list: Vec::new(),
            window_weight: 0,
            probation_weight: 0,
            protected_weight: 0,
            max_total,
            max_window,
            max_protected,
        }
    }

    // -----------------------------------------------------------------------
    // Hashing
    // -----------------------------------------------------------------------

    #[inline]
    fn hash_key(&self, key: &K) -> u64 {
        self.build_hasher.hash_one(key)
    }

    /// Returns the `RandomState` used by this policy's sketch and doorkeeper.
    ///
    /// The cache layer clones this once at construction time so it can hash
    /// keys on the read path **without** holding the policy lock.
    #[allow(dead_code)]
    pub(crate) fn hasher(&self) -> &RandomState {
        &self.build_hasher
    }

    // -----------------------------------------------------------------------
    // Linked-list helpers (operate on the arena by index)
    // -----------------------------------------------------------------------

    /// Inserts node `idx` immediately after sentinel `head` (MRU position).
    #[inline]
    fn link_after(&mut self, head: usize, idx: usize) {
        let old_first = self.nodes[head].next;
        self.nodes[idx].prev = head;
        self.nodes[idx].next = old_first;
        self.nodes[head].next = idx;
        self.nodes[old_first].prev = idx;
    }

    /// Removes node `idx` from its current position.
    /// After this call `nodes[idx].{prev, next} == NULL`.
    #[inline]
    fn unlink(&mut self, idx: usize) {
        let prev = self.nodes[idx].prev;
        let next = self.nodes[idx].next;
        self.nodes[prev].next = next;
        self.nodes[next].prev = prev;
        self.nodes[idx].prev = NULL;
        self.nodes[idx].next = NULL;
    }

    // -----------------------------------------------------------------------
    // Node lifecycle
    // -----------------------------------------------------------------------

    fn alloc_node(&mut self, key: K, key_hash: u64, weight: u64, queue: Queue) -> usize {
        if let Some(idx) = self.free_list.pop() {
            let n = &mut self.nodes[idx];
            n.key = Some(key);
            n.key_hash = key_hash;
            n.weight = weight;
            n.prev = NULL;
            n.next = NULL;
            n.queue = queue;
            idx
        } else {
            let idx = self.nodes.len();
            self.nodes.push(TlfuNode {
                key: Some(key),
                key_hash,
                weight,
                prev: NULL,
                next: NULL,
                queue,
            });
            idx
        }
    }

    /// Evicts a node that is **already unlinked** from its queue.
    ///
    /// Removes the key from `self.index` and recycles the slot.
    /// Returns the owned key so the caller can remove it from the store.
    fn evict_detached(&mut self, idx: usize) -> Option<K> {
        let key = self.nodes[idx].key.take()?;
        self.index.remove(&key);
        self.free_list.push(idx);
        Some(key)
    }

    /// Evicts a node that is **still linked** in one of the queues.
    fn evict_linked(&mut self, idx: usize) -> Option<K> {
        let w = self.nodes[idx].weight;
        match self.nodes[idx].queue {
            Queue::Window => self.window_weight -= w,
            Queue::Probation => self.probation_weight -= w,
            Queue::Protected => self.protected_weight -= w,
        }
        self.unlink(idx);
        self.evict_detached(idx)
    }

    // -----------------------------------------------------------------------
    // Frequency accounting
    // -----------------------------------------------------------------------

    /// Updates frequency information for `h` using the doorkeeper filter.
    ///
    /// - First sighting: recorded in doorkeeper only (sketch unchanged).
    /// - Subsequent sightings: doorkeeper already set → sketch incremented.
    /// - When the sketch resets internally, the doorkeeper is also cleared
    ///   so both data structures remain consistent.
    #[inline]
    fn record_frequency(&mut self, h: u64) {
        // (body shared with the public wrapper below)
        self.record_access_hash(h);
    }

    /// Public wrapper around `record_frequency`, called by the maintenance
    /// path when draining the read buffer (which stores hashes, not keys).
    #[inline]
    pub(crate) fn record_access_hash(&mut self, h: u64) {
        if self.doorkeeper.insert(h) {
            let prev_additions = self.sketch.additions;
            self.sketch.increment(h);
            // Detect whether the sketch just triggered an internal reset
            // (additions counter was halved).  If so, flush the doorkeeper
            // so first-seen information does not outlive sketch data.
            if self.sketch.additions < prev_additions {
                self.doorkeeper.clear();
            }
        }
    }

    /// Returns the estimated frequency of the key stored at `idx`.
    #[inline]
    fn node_freq(&self, idx: usize) -> u8 {
        self.sketch.frequency(self.nodes[idx].key_hash)
    }

    // -----------------------------------------------------------------------
    // Promotion / demotion
    // -----------------------------------------------------------------------

    /// Promotes `idx` from Probation to Protected (MRU position).
    ///
    /// If Protected overflows, its LRU entry is demoted back to Probation.
    fn promote_to_protected(&mut self, idx: usize) {
        debug_assert_eq!(self.nodes[idx].queue, Queue::Probation);
        let w = self.nodes[idx].weight;
        self.unlink(idx);
        self.probation_weight -= w;

        self.link_after(PROTECTED_HEAD, idx);
        self.nodes[idx].queue = Queue::Protected;
        self.protected_weight += w;

        // Demote LRU Protected entries until we are within capacity.
        while self.protected_weight > self.max_protected {
            let demote_idx = self.nodes[PROTECTED_TAIL].prev;
            if demote_idx == PROTECTED_HEAD {
                break; // protected segment is empty
            }
            let dw = self.nodes[demote_idx].weight;
            self.unlink(demote_idx);
            self.protected_weight -= dw;

            self.link_after(PROBATION_HEAD, demote_idx);
            self.nodes[demote_idx].queue = Queue::Probation;
            self.probation_weight += dw;
        }
    }

    // -----------------------------------------------------------------------
    // Core admission / eviction logic
    // -----------------------------------------------------------------------

    /// After placing a new entry (or updating an existing one) in the Window,
    /// cascade any overflow from Window → Main → eviction.
    ///
    /// Returns the list of keys that must be removed from the backing store.
    fn drain_to_capacity(&mut self) -> Vec<K> {
        let mut evicted = Vec::new();

        while self.window_weight > self.max_window {
            // Select the LRU (least-recently-used) entry in the Window.
            let cand = self.nodes[WINDOW_TAIL].prev;
            if cand == WINDOW_HEAD {
                break; // window empty — shouldn't happen
            }

            let cand_weight = self.nodes[cand].weight;
            self.unlink(cand);
            self.window_weight -= cand_weight;
            // Tentatively mark as Probation; updated below if rejected.
            self.nodes[cand].queue = Queue::Probation;

            let main_weight = self.probation_weight + self.protected_weight;
            let max_main = self.max_total - self.max_window;

            if main_weight + cand_weight <= max_main {
                // Main has room — unconditionally admit the candidate.
                self.link_after(PROBATION_HEAD, cand);
                self.probation_weight += cand_weight;
            } else {
                // Main is full — run the TinyLFU admission filter.
                let victim = self.nodes[PROBATION_TAIL].prev;

                if victim == PROBATION_HEAD {
                    // Probation is empty; admit the candidate regardless.
                    self.link_after(PROBATION_HEAD, cand);
                    self.probation_weight += cand_weight;
                } else {
                    let cand_freq = self.node_freq(cand);
                    let victim_freq = self.node_freq(victim);

                    if cand_freq > victim_freq {
                        // Candidate wins: evict the victim, admit the candidate.
                        let vw = self.nodes[victim].weight;
                        self.unlink(victim);
                        self.probation_weight -= vw;
                        if let Some(k) = self.evict_detached(victim) {
                            evicted.push(k);
                        }
                        self.link_after(PROBATION_HEAD, cand);
                        self.probation_weight += cand_weight;
                    } else {
                        // Victim wins (tie goes to the incumbent): evict the candidate.
                        if let Some(k) = self.evict_detached(cand) {
                            evicted.push(k);
                        }
                    }
                }
            }
        }

        evicted
    }
}

// ---------------------------------------------------------------------------
// Policy trait implementation
// ---------------------------------------------------------------------------

impl<K: Hash + Eq + Clone + Send> Policy<K> for WTinyLfuPolicy<K> {
    /// Called on every cache hit (read path).
    ///
    /// 1. Records the access in the frequency sketch (via doorkeeper).
    /// 2. Updates LRU ordering within the entry's queue:
    ///    - Window entry → moves to Window MRU.
    ///    - Probation entry → **promoted** to Protected MRU.
    ///    - Protected entry → moves to Protected MRU.
    fn on_access(&mut self, key: &K) {
        let h = self.hash_key(key);
        self.record_frequency(h);

        if let Some(&idx) = self.index.get(key) {
            match self.nodes[idx].queue {
                Queue::Probation => {
                    self.promote_to_protected(idx);
                }
                Queue::Window => {
                    self.unlink(idx);
                    self.link_after(WINDOW_HEAD, idx);
                }
                Queue::Protected => {
                    self.unlink(idx);
                    self.link_after(PROTECTED_HEAD, idx);
                }
            }
        }
    }

    /// Called when a new entry is inserted into the cache.
    ///
    /// Places the entry at the Window MRU and cascades any overflow through
    /// the admission filter.  Returns keys that must be removed from the
    /// backing store.
    ///
    /// If the key already exists (re-insertion / value replacement), the
    /// entry is moved to the Window MRU and the weight is updated.
    fn on_insert(&mut self, key: K, weight: u64) -> Vec<K> {
        let h = self.hash_key(&key);

        if let Some(&idx) = self.index.get(&key) {
            // ---- Key already tracked: update in place ----
            let old_weight = self.nodes[idx].weight;
            self.nodes[idx].weight = weight;
            self.nodes[idx].key_hash = h;

            // Adjust the queue weight and move to MRU.
            match self.nodes[idx].queue {
                Queue::Window => {
                    self.window_weight = self.window_weight - old_weight + weight;
                    self.unlink(idx);
                    self.link_after(WINDOW_HEAD, idx);
                }
                Queue::Probation => {
                    self.probation_weight = self.probation_weight - old_weight + weight;
                    // Keep in probation but refresh position.
                    self.unlink(idx);
                    self.link_after(PROBATION_HEAD, idx);
                }
                Queue::Protected => {
                    self.protected_weight = self.protected_weight - old_weight + weight;
                    self.unlink(idx);
                    self.link_after(PROTECTED_HEAD, idx);
                }
            }
            return self.drain_to_capacity();
        }

        // ---- Brand-new entry: place at Window MRU ----
        let idx = self.alloc_node(key.clone(), h, weight, Queue::Window);
        self.index.insert(key, idx);
        self.link_after(WINDOW_HEAD, idx);
        self.window_weight += weight;

        self.drain_to_capacity()
    }

    /// Called when an existing entry's value (and potentially weight) changes.
    ///
    /// If the weight is unchanged this is equivalent to a recency update
    /// (move to MRU).  If the weight changed we re-run the capacity check.
    fn on_update(&mut self, key: &K, _old_weight: u64, new_weight: u64) -> Vec<K> {
        if let Some(&idx) = self.index.get(key) {
            let old_w = self.nodes[idx].weight;
            if old_w == new_weight {
                // Weight unchanged: just refresh recency.
                self.on_access(key);
                return vec![];
            }
            // Weight changed: update in place and re-check capacity.
            self.nodes[idx].weight = new_weight;
            match self.nodes[idx].queue {
                Queue::Window => {
                    self.window_weight = self.window_weight - old_w + new_weight;
                }
                Queue::Probation => {
                    self.probation_weight = self.probation_weight - old_w + new_weight;
                }
                Queue::Protected => {
                    self.protected_weight = self.protected_weight - old_w + new_weight;
                }
            }
            self.drain_to_capacity()
        } else {
            vec![]
        }
    }

    /// Called when an entry is explicitly removed by the user.
    fn on_remove(&mut self, key: &K) {
        if let Some(idx) = self.index.get(key).copied() {
            self.evict_linked(idx);
        }
    }

    fn current_weight(&self) -> u64 {
        self.window_weight + self.probation_weight + self.protected_weight
    }

    fn max_weight(&self) -> u64 {
        self.max_total
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn make(cap: u64) -> WTinyLfuPolicy<u64> {
        WTinyLfuPolicy::new(cap)
    }

    #[test]
    fn insert_and_remove() {
        let mut p = make(10);
        let evicted = p.on_insert(1u64, 1);
        assert!(evicted.is_empty());
        assert_eq!(p.current_weight(), 1);
        p.on_remove(&1u64);
        assert_eq!(p.current_weight(), 0);
    }

    #[test]
    fn capacity_is_respected() {
        let cap = 20u64;
        let mut p = make(cap);
        for i in 0..50u64 {
            p.on_insert(i, 1);
        }
        assert!(
            p.current_weight() <= cap,
            "weight {} exceeds capacity {}",
            p.current_weight(),
            cap
        );
    }

    #[test]
    fn duplicate_insert_does_not_grow_weight() {
        let mut p = make(10);
        p.on_insert(42u64, 1);
        p.on_insert(42u64, 1); // same key
        assert_eq!(p.current_weight(), 1, "duplicate insert must not double weight");
    }

    #[test]
    fn on_remove_unknown_key_is_noop() {
        let mut p = make(10);
        p.on_remove(&999u64); // must not panic
        assert_eq!(p.current_weight(), 0);
    }

    #[test]
    fn hot_items_survive_scan_pollution() {
        // Classic W-TinyLFU property: frequently accessed items should
        // withstand a flood of cold (one-hit) insertions.
        let cap = 50u64;
        let mut p = make(cap);

        // Insert and warm up 20 hot keys.
        for i in 0..20u64 {
            p.on_insert(i, 1);
        }
        for _ in 0..8 {
            for i in 0..20u64 {
                p.on_access(&i);
            }
        }

        // Scan: insert 300 cold one-hit-wonder keys.
        for i in 1000..1300u64 {
            p.on_insert(i, 1);
        }

        let survivors = (0..20u64).filter(|k| p.index.contains_key(k)).count();
        assert!(
            survivors >= 10,
            "only {} / 20 hot items survived the scan",
            survivors
        );
    }

    #[test]
    fn probation_entry_is_promoted_on_access() {
        // Use a large enough capacity so window overflow pushes items to probation.
        let mut p = make(100);
        for i in 0..50u64 {
            p.on_insert(i, 1);
        }
        // Build frequency for key 0 so doorkeeper passes it to sketch.
        p.on_access(&0u64);
        p.on_access(&0u64);

        // If key 0 is in probation, another access should promote it.
        if let Some(&idx) = p.index.get(&0u64) {
            if p.nodes[idx].queue == Queue::Probation {
                p.on_access(&0u64);
                let idx2 = p.index[&0u64];
                assert_eq!(
                    p.nodes[idx2].queue,
                    Queue::Protected,
                    "key 0 should have been promoted to Protected"
                );
            }
        }
    }
}
