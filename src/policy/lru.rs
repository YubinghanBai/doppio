use std::hash::Hash;
use ahash::AHashMap;
use super::Policy;

/// Sentinel indices in the `nodes` arena.
const HEAD: usize = 0; // most-recently-used end
const TAIL: usize = 1; // least-recently-used end
const NULL: usize = usize::MAX;

struct LruNode<K> {
    /// `None` only for the HEAD and TAIL sentinels.
    key: Option<K>,
    weight: u64,
    /// Index toward HEAD (more recently used).
    prev: usize,
    /// Index toward TAIL (less recently used).
    next: usize,
}

/// O(1) LRU policy backed by an index-arena doubly-linked list.
///
/// Nodes are stored in a `Vec<LruNode<K>>` and linked by index, avoiding
/// unsafe raw pointers in Phase 1 at the cost of a little indirection.
pub struct LruPolicy<K> {
    /// Index 0 = HEAD sentinel, 1 = TAIL sentinel, 2+ = real entries.
    nodes: Vec<LruNode<K>>,
    /// Maps a key to its index in `nodes`.
    map: AHashMap<K, usize>,
    /// Indices of freed (reusable) slots.
    free_list: Vec<usize>,
    total_weight: u64,
    max_weight: u64,
}

impl<K: Hash + Eq + Clone + Send> LruPolicy<K> {
    /// Creates a new `LruPolicy` with the given maximum total weight.
    pub fn new(max_weight: u64) -> Self {
        let mut nodes: Vec<LruNode<K>> = Vec::with_capacity(16);
        // HEAD sentinel (index 0): next points to TAIL initially
        nodes.push(LruNode {
            key: None,
            weight: 0,
            prev: NULL,
            next: TAIL,
        });
        // TAIL sentinel (index 1): prev points to HEAD initially
        nodes.push(LruNode {
            key: None,
            weight: 0,
            prev: HEAD,
            next: NULL,
        });

        LruPolicy {
            nodes,
            map: AHashMap::new(),
            free_list: Vec::new(),
            total_weight: 0,
            max_weight,
        }
    }

    /// Links `idx` immediately after the HEAD sentinel (marks it most-recently-used).
    fn link_after_head(&mut self, idx: usize) {
        let old_first = self.nodes[HEAD].next;
        self.nodes[idx].prev = HEAD;
        self.nodes[idx].next = old_first;
        self.nodes[HEAD].next = idx;
        self.nodes[old_first].prev = idx;
    }

    /// Detaches `idx` from its current position in the list.
    fn unlink(&mut self, idx: usize) {
        let prev = self.nodes[idx].prev;
        let next = self.nodes[idx].next;
        self.nodes[prev].next = next;
        self.nodes[next].prev = prev;
        self.nodes[idx].prev = NULL;
        self.nodes[idx].next = NULL;
    }

    /// Allocates a new node (reusing from the free list when available).
    fn alloc_node(&mut self, key: K, weight: u64) -> usize {
        if let Some(idx) = self.free_list.pop() {
            self.nodes[idx].key = Some(key);
            self.nodes[idx].weight = weight;
            self.nodes[idx].prev = NULL;
            self.nodes[idx].next = NULL;
            idx
        } else {
            let idx = self.nodes.len();
            self.nodes.push(LruNode {
                key: Some(key),
                weight,
                prev: NULL,
                next: NULL,
            });
            idx
        }
    }

    /// Returns the key and weight of the least-recently-used entry and removes it.
    fn evict_lru(&mut self) -> Option<(K, u64)> {
        let lru_idx = self.nodes[TAIL].prev;
        if lru_idx == HEAD {
            return None; // list is empty
        }
        self.unlink(lru_idx);
        let key = self.nodes[lru_idx].key.take()?;
        let weight = self.nodes[lru_idx].weight;
        self.map.remove(&key);
        self.free_list.push(lru_idx);
        Some((key, weight))
    }

    /// Drains evictions until `total_weight <= max_weight`.
    fn drain_evictions(&mut self) -> Vec<K> {
        let mut evicted = Vec::new();
        while self.total_weight > self.max_weight {
            match self.evict_lru() {
                Some((k, w)) => {
                    self.total_weight -= w;
                    evicted.push(k);
                }
                None => break,
            }
        }
        evicted
    }
}

impl<K: Hash + Eq + Clone + Send> Policy<K> for LruPolicy<K> {
    fn on_access(&mut self, key: &K) {
        if let Some(&idx) = self.map.get(key) {
            self.unlink(idx);
            self.link_after_head(idx);
        }
    }

    fn on_insert(&mut self, key: K, weight: u64) -> Vec<K> {
        if let Some(&idx) = self.map.get(&key) {
            // Key already tracked — treat as update
            let old_weight = self.nodes[idx].weight;
            self.nodes[idx].weight = weight;
            self.total_weight = self.total_weight - old_weight + weight;
            self.unlink(idx);
            self.link_after_head(idx);
        } else {
            let idx = self.alloc_node(key.clone(), weight);
            self.map.insert(key, idx);
            self.link_after_head(idx);
            self.total_weight += weight;
        }
        self.drain_evictions()
    }

    fn on_update(&mut self, key: &K, old_weight: u64, new_weight: u64) -> Vec<K> {
        if let Some(&idx) = self.map.get(key) {
            self.nodes[idx].weight = new_weight;
            self.total_weight = self.total_weight - old_weight + new_weight;
            self.unlink(idx);
            self.link_after_head(idx);
        }
        self.drain_evictions()
    }

    fn on_remove(&mut self, key: &K) {
        if let Some(idx) = self.map.remove(key) {
            let weight = self.nodes[idx].weight;
            self.unlink(idx);
            self.nodes[idx].key = None;
            self.free_list.push(idx);
            self.total_weight -= weight;
        }
    }

    fn current_weight(&self) -> u64 {
        self.total_weight
    }

    fn max_weight(&self) -> u64 {
        self.max_weight
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn evicts_lru_entry_when_full() {
        let mut policy: LruPolicy<&str> = LruPolicy::new(2);
        assert!(policy.on_insert("a", 1).is_empty());
        assert!(policy.on_insert("b", 1).is_empty());
        let evicted = policy.on_insert("c", 1);
        assert_eq!(evicted, vec!["a"]); // "a" is LRU
    }

    #[test]
    fn access_promotes_to_mru() {
        let mut policy: LruPolicy<&str> = LruPolicy::new(2);
        policy.on_insert("a", 1);
        policy.on_insert("b", 1);
        policy.on_access(&"a"); // "a" is now MRU, "b" is LRU
        let evicted = policy.on_insert("c", 1);
        assert_eq!(evicted, vec!["b"]); // "b" is evicted
    }

    #[test]
    fn on_remove_decrements_weight() {
        let mut policy: LruPolicy<&str> = LruPolicy::new(3);
        policy.on_insert("a", 1);
        policy.on_insert("b", 1);
        policy.on_remove(&"a");
        assert_eq!(policy.current_weight(), 1);
        assert_eq!(policy.on_insert("c", 1).len(), 0);
        assert_eq!(policy.on_insert("d", 1).len(), 0); // still under cap=3
    }
}
