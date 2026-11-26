//! Bounded least-recently-used storage with O(1) promotion and eviction.
//!
//! Entries live in a slab and are linked into one intrusive doubly linked
//! recency list; an ordered index maps each key to its slab slot. The key is
//! shared (`Arc`) between the index and the slab node, so a cache hit
//! promotes an entry without allocating, comparing more than the O(log n)
//! index lookup, or growing any queue. An insertion allocates exactly one
//! `Arc` and never clones the key.
//!
//! Capacity is two-dimensional: a maximum entry count and a maximum total
//! weight (bytes for byte-budgeted caches). Replacing an existing key first
//! releases the old entry, so a replacement that still fits never evicts an
//! unrelated entry; a value heavier than the whole budget is refused and
//! leaves no stale predecessor behind.

use alloc::collections::BTreeMap;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::borrow::Borrow;
use core::cmp::Ordering;

const NIL: u32 = u32::MAX;

/// Index key whose ordering is exactly the ordering of the shared `K`, so
/// lookups by `&K` need no clone.
struct SharedKey<K>(Arc<K>);

impl<K> Borrow<K> for SharedKey<K> {
    fn borrow(&self) -> &K {
        &self.0
    }
}
impl<K: Ord> PartialEq for SharedKey<K> {
    fn eq(&self, other: &Self) -> bool {
        self.0 == other.0
    }
}
impl<K: Ord> Eq for SharedKey<K> {}
impl<K: Ord> PartialOrd for SharedKey<K> {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}
impl<K: Ord> Ord for SharedKey<K> {
    fn cmp(&self, other: &Self) -> Ordering {
        self.0.cmp(&other.0)
    }
}

enum Slot<K, V> {
    Free { next_free: u32 },
    Used(Node<K, V>),
}

struct Node<K, V> {
    key: Arc<K>,
    value: V,
    weight: usize,
    /// Toward the most recently used end.
    newer: u32,
    /// Toward the least recently used end.
    older: u32,
}

pub struct LruCache<K: Ord, V> {
    slots: Vec<Slot<K, V>>,
    index: BTreeMap<SharedKey<K>, u32>,
    free_head: u32,
    /// Most recently used.
    head: u32,
    /// Least recently used.
    tail: u32,
    weight: usize,
    max_entries: usize,
    max_weight: usize,
}

impl<K: Ord, V> LruCache<K, V> {
    pub const fn new(max_entries: usize, max_weight: usize) -> Self {
        Self {
            slots: Vec::new(),
            index: BTreeMap::new(),
            free_head: NIL,
            head: NIL,
            tail: NIL,
            weight: 0,
            max_entries,
            max_weight,
        }
    }

    pub fn len(&self) -> usize {
        self.index.len()
    }

    pub fn is_empty(&self) -> bool {
        self.index.is_empty()
    }

    /// Total weight of the resident entries.
    pub fn weight(&self) -> usize {
        self.weight
    }

    pub fn contains_key(&self, key: &K) -> bool {
        self.index.contains_key(key)
    }

    /// Promotes `key` to most recently used and returns its value.
    pub fn get(&mut self, key: &K) -> Option<&V> {
        let slot = *self.index.get(key)?;
        if slot != self.head {
            self.unlink(slot);
            self.push_front(slot);
        }
        Some(&self.node(slot).value)
    }

    /// Looks up without changing recency.
    #[cfg(test)]
    pub fn peek(&self, key: &K) -> Option<&V> {
        let slot = *self.index.get(key)?;
        Some(&self.node(slot).value)
    }

    /// Inserts or replaces `key`, evicting least recently used entries (each
    /// passed to `evicted`) until the entry and weight limits hold. Returns the
    /// replaced value, or gives the value back if `weight` alone exceeds the
    /// budget (any previous entry for `key` is still removed, never left
    /// stale).
    pub fn insert_with(
        &mut self,
        key: K,
        value: V,
        weight: usize,
        mut evicted: impl FnMut(K, V),
    ) -> Result<Option<V>, V> {
        let replaced = self.remove(&key);
        if weight > self.max_weight || self.max_entries == 0 {
            return match replaced {
                // The caller learns the old value left the cache through the
                // eviction channel; the new one is handed back.
                Some(old) => {
                    evicted(key, old);
                    Err(value)
                }
                None => Err(value),
            };
        }
        while self.index.len() >= self.max_entries
            || self.weight.saturating_add(weight) > self.max_weight
        {
            match self.pop_lru() {
                Some((old_key, old_value)) => evicted(old_key, old_value),
                None => break,
            }
        }
        let key = Arc::new(key);
        let slot = self.alloc(Node { key: key.clone(), value, weight, newer: NIL, older: NIL });
        self.index.insert(SharedKey(key), slot);
        self.weight += weight;
        self.push_front(slot);
        Ok(replaced)
    }

    /// `insert_with` that drops evicted entries.
    pub fn insert(&mut self, key: K, value: V, weight: usize) -> Result<Option<V>, V> {
        self.insert_with(key, value, weight, |_, _| {})
    }

    pub fn remove(&mut self, key: &K) -> Option<V> {
        let slot = self.index.remove(key)?;
        self.unlink(slot);
        Some(self.release(slot).1)
    }

    /// Removes and returns the least recently used entry.
    pub fn pop_lru(&mut self) -> Option<(K, V)> {
        if self.tail == NIL {
            return None;
        }
        let slot = self.tail;
        let shared = self.node(slot).key.clone();
        self.index.remove(&*shared);
        self.unlink(slot);
        drop(shared);
        Some(self.release(slot))
    }

    pub fn clear(&mut self) {
        self.slots.clear();
        self.index.clear();
        self.free_head = NIL;
        self.head = NIL;
        self.tail = NIL;
        self.weight = 0;
    }

    fn node(&self, slot: u32) -> &Node<K, V> {
        match &self.slots[slot as usize] {
            Slot::Used(node) => node,
            Slot::Free { .. } => unreachable!("linked slot is occupied"),
        }
    }

    fn node_mut(&mut self, slot: u32) -> &mut Node<K, V> {
        match &mut self.slots[slot as usize] {
            Slot::Used(node) => node,
            Slot::Free { .. } => unreachable!("linked slot is occupied"),
        }
    }

    fn alloc(&mut self, node: Node<K, V>) -> u32 {
        if self.free_head != NIL {
            let slot = self.free_head;
            let Slot::Free { next_free } = self.slots[slot as usize] else {
                unreachable!("free list holds free slots")
            };
            self.free_head = next_free;
            self.slots[slot as usize] = Slot::Used(node);
            slot
        } else {
            let slot = u32::try_from(self.slots.len()).expect("slab index fits u32");
            assert!(slot != NIL, "slab full");
            self.slots.push(Slot::Used(node));
            slot
        }
    }

    /// Frees an already unlinked, already de-indexed slot.
    fn release(&mut self, slot: u32) -> (K, V) {
        let freed = core::mem::replace(
            &mut self.slots[slot as usize],
            Slot::Free { next_free: self.free_head },
        );
        self.free_head = slot;
        let Slot::Used(node) = freed else { unreachable!("released slot was occupied") };
        self.weight -= node.weight;
        let key = Arc::try_unwrap(node.key).unwrap_or_else(|_| unreachable!("index entry already dropped"));
        (key, node.value)
    }

    fn unlink(&mut self, slot: u32) {
        let (newer, older) = {
            let node = self.node(slot);
            (node.newer, node.older)
        };
        if newer == NIL {
            self.head = older;
        } else {
            self.node_mut(newer).older = older;
        }
        if older == NIL {
            self.tail = newer;
        } else {
            self.node_mut(older).newer = newer;
        }
    }

    fn push_front(&mut self, slot: u32) {
        let old_head = self.head;
        {
            let node = self.node_mut(slot);
            node.newer = NIL;
            node.older = old_head;
        }
        if old_head == NIL {
            self.tail = slot;
        } else {
            self.node_mut(old_head).newer = slot;
        }
        self.head = slot;
    }
}

/// Recency order for caches whose values live elsewhere (a map keyed by the
/// same key). It shares `LruCache`'s O(1) list; a hit allocates nothing.
pub struct LruIndex<K: Ord + Clone> {
    order: LruCache<K, ()>,
}

impl<K: Ord + Clone> LruIndex<K> {
    pub const fn new() -> Self {
        Self { order: LruCache::new(usize::MAX, usize::MAX) }
    }

    /// Marks `key` most recently used, tracking it if it was not.
    pub fn touch(&mut self, key: &K) {
        if self.order.get(key).is_none() {
            let _ = self.order.insert(key.clone(), (), 0);
        }
    }

    pub fn remove(&mut self, key: &K) {
        self.order.remove(key);
    }

    pub fn pop_lru(&mut self) -> Option<K> {
        self.order.pop_lru().map(|(key, ())| key)
    }

    pub fn clear(&mut self) {
        self.order.clear();
    }

    pub fn len(&self) -> usize {
        self.order.len()
    }

    pub fn is_empty(&self) -> bool {
        self.order.is_empty()
    }
}

impl<K: Ord + Clone> Default for LruIndex<K> {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::string::{String, ToString};
    use alloc::vec;

    fn keys(cache: &mut LruCache<String, u32>) -> Vec<String> {
        let mut out = Vec::new();
        while let Some((key, _)) = cache.pop_lru() {
            out.push(key);
        }
        out
    }

    #[test]
    fn hit_promotes_and_miss_reports_none() {
        let mut cache = LruCache::new(8, usize::MAX);
        for (index, name) in ["a", "b", "c"].into_iter().enumerate() {
            cache.insert(name.to_string(), index as u32, 1).unwrap();
        }
        assert_eq!(cache.get(&"a".to_string()), Some(&0));
        assert_eq!(cache.get(&"zz".to_string()), None);
        assert_eq!(cache.peek(&"b".to_string()), Some(&1));
        assert_eq!(keys(&mut cache), vec!["b", "c", "a"]);
    }

    #[test]
    fn entry_cap_evicts_least_recently_used_in_order() {
        let mut cache = LruCache::new(3, usize::MAX);
        let mut evicted = Vec::new();
        for name in ["a", "b", "c"] {
            cache.insert(name.to_string(), 0, 1).unwrap();
        }
        cache.get(&"a".to_string());
        for name in ["d", "e"] {
            cache.insert_with(name.to_string(), 0, 1, |key, _| evicted.push(key)).unwrap();
        }
        assert_eq!(evicted, vec!["b", "c"]);
        assert_eq!(cache.len(), 3);
        assert!(cache.contains_key(&"a".to_string()));
    }

    #[test]
    fn replacing_a_key_never_evicts_an_unrelated_entry() {
        let mut cache = LruCache::new(2, usize::MAX);
        cache.insert("a".to_string(), 1, 1).unwrap();
        cache.insert("b".to_string(), 2, 1).unwrap();
        let mut evicted = Vec::new();
        let old = cache.insert_with("a".to_string(), 9, 1, |key, _| evicted.push(key)).unwrap();
        assert_eq!(old, Some(1));
        assert!(evicted.is_empty());
        assert_eq!(cache.len(), 2);
        assert_eq!(cache.peek(&"a".to_string()), Some(&9));
        assert_eq!(cache.peek(&"b".to_string()), Some(&2));
        // The replacement is the newest entry.
        assert_eq!(keys(&mut cache), vec!["b", "a"]);
    }

    #[test]
    fn byte_budget_counts_weights_and_replacement_rebalances() {
        let mut cache = LruCache::new(usize::MAX, 100);
        cache.insert("a".to_string(), 0, 60).unwrap();
        cache.insert("b".to_string(), 0, 40).unwrap();
        assert_eq!(cache.weight(), 100);
        // Shrinking a replacement frees budget instead of evicting.
        cache.insert("a".to_string(), 0, 10).unwrap();
        assert_eq!((cache.len(), cache.weight()), (2, 50));
        // Growing it evicts only as many LRU entries as needed ("b" is older).
        let mut evicted = Vec::new();
        cache.insert_with("a".to_string(), 0, 90, |key, _| evicted.push(key)).unwrap();
        assert_eq!(evicted, vec!["b"]);
        assert_eq!(cache.weight(), 90);
        // A new entry that needs room pushes out the oldest first.
        cache.insert_with("c".to_string(), 0, 30, |key, _| evicted.push(key)).unwrap();
        assert_eq!(evicted, vec!["b", "a"]);
        assert_eq!(cache.weight(), 30);
    }

    #[test]
    fn over_budget_value_is_refused_and_drops_the_stale_predecessor() {
        let mut cache = LruCache::new(4, 10);
        cache.insert("a".to_string(), 1, 5).unwrap();
        let mut evicted = Vec::new();
        assert_eq!(cache.insert_with("a".to_string(), 2, 11, |key, _| evicted.push(key)), Err(2));
        assert_eq!(evicted, vec!["a"]);
        assert!(cache.is_empty());
        assert_eq!(cache.weight(), 0);
    }

    #[test]
    fn remove_relinks_and_slots_are_reused_without_growth() {
        let mut cache = LruCache::new(usize::MAX, usize::MAX);
        for round in 0..1000u32 {
            for name in ["a", "b", "c"] {
                cache.insert(name.to_string(), round, 1).unwrap();
            }
            assert_eq!(cache.remove(&"b".to_string()), Some(round));
            assert_eq!(cache.remove(&"b".to_string()), None);
            assert_eq!(cache.pop_lru().map(|(key, _)| key), Some("a".to_string()));
            assert_eq!(cache.remove(&"c".to_string()), Some(round));
            assert!(cache.is_empty());
        }
        assert!(cache.slots.len() <= 3);
        assert_eq!(cache.weight(), 0);
    }

    #[test]
    fn clear_resets_every_structure() {
        let mut cache = LruCache::new(2, 10);
        cache.insert("a".to_string(), 1, 4).unwrap();
        cache.clear();
        assert!(cache.is_empty() && cache.pop_lru().is_none());
        cache.insert("a".to_string(), 1, 10).unwrap();
        assert_eq!(cache.weight(), 10);
    }

    #[test]
    fn index_touch_orders_by_recency_and_hits_do_not_grow_it() {
        let mut index = LruIndex::new();
        for name in ["a", "b", "c"] {
            index.touch(&name.to_string());
        }
        for _ in 0..10_000 {
            index.touch(&"a".to_string());
        }
        assert_eq!(index.len(), 3);
        index.remove(&"c".to_string());
        index.remove(&"missing".to_string());
        assert_eq!(index.pop_lru(), Some("b".to_string()));
        assert_eq!(index.pop_lru(), Some("a".to_string()));
        assert_eq!(index.pop_lru(), None);
        index.touch(&"x".to_string());
        index.clear();
        assert!(index.is_empty());
    }
}
