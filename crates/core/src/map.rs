//! Flat open-addressing hash table — the index at the heart of the cache.
//!
//! Design (see ADR-001, research/01):
//! - Open addressing with **linear probing**. Byte-string keys, generic value `V`.
//! - **Tombstone-free**: deletion uses backward-shift, so probe sequences never accumulate
//!   tombstones (which otherwise degrade lookups under churn — a real Redis-vs-flat win).
//! - Power-of-two capacity, masked indexing, stored hashes to make resize and probing cheap.
//!
//! This is the correctness-first foundation. The next optimization step (ADR-001 D5) is
//! SwissTable-style control bytes with SIMD group scans and, beyond that, dashtable
//! extendible-hashing segments for incremental, spike-free resize. The public API here is
//! shaped so those can be swapped in underneath without changing callers.

use core::mem;

/// Target max load factor = 7/8. We grow before crossing it to keep probe chains short.
const LOAD_NUM: usize = 7;
const LOAD_DEN: usize = 8;

/// Smallest non-empty capacity (power of two).
const MIN_CAP: usize = 16;

struct Entry<V> {
    hash: u64,
    key: Box<[u8]>,
    value: V,
}

/// A byte-keyed flat hash map with generic values.
///
/// Not thread-safe by itself — in the thread-per-core design each core owns its shards, so the
/// map is accessed single-threaded on the hot path (no locking overhead).
pub struct FlatMap<V> {
    /// Slots: `None` is empty. With backward-shift deletion we never need a tombstone state.
    slots: Vec<Option<Entry<V>>>,
    len: usize,
    /// `capacity - 1`; capacity is always a power of two so this is the index mask.
    mask: usize,
    hasher: ahash::RandomState,
}

impl<V> Default for FlatMap<V> {
    fn default() -> Self {
        Self::new()
    }
}

impl<V> FlatMap<V> {
    /// Create an empty map. No allocation until the first insert.
    pub fn new() -> Self {
        FlatMap {
            slots: Vec::new(),
            len: 0,
            mask: 0,
            hasher: ahash::RandomState::new(),
        }
    }

    /// Create a map pre-sized to hold at least `cap` entries without resizing.
    pub fn with_capacity(cap: usize) -> Self {
        let mut m = Self::new();
        if cap > 0 {
            let raw = ((cap * LOAD_DEN) / LOAD_NUM + 1)
                .next_power_of_two()
                .max(MIN_CAP);
            m.alloc(raw);
        }
        m
    }

    #[inline]
    pub fn len(&self) -> usize {
        self.len
    }

    #[inline]
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Number of slots currently allocated.
    #[inline]
    pub fn capacity(&self) -> usize {
        self.slots.len()
    }

    #[inline]
    fn hash(&self, key: &[u8]) -> u64 {
        self.hasher.hash_one(key)
    }

    fn alloc(&mut self, raw_cap: usize) {
        debug_assert!(raw_cap.is_power_of_two());
        self.slots = (0..raw_cap).map(|_| None).collect();
        self.mask = raw_cap - 1;
    }

    /// Threshold count at which we must grow (grow before exceeding it).
    #[inline]
    fn grow_threshold(&self) -> usize {
        (self.slots.len() / LOAD_DEN) * LOAD_NUM
    }

    /// Compute the hash for a key (callers can cache it and pass it to the `*_pre` variants to
    /// avoid hashing twice — e.g. once for shard routing and once for the bucket).
    #[inline]
    pub fn hash_key(&self, key: &[u8]) -> u64 {
        self.hash(key)
    }

    /// Look up a value by key.
    #[inline]
    pub fn get(&self, key: &[u8]) -> Option<&V> {
        self.get_pre(self.hash(key), key)
    }

    /// Look up a value by key using a precomputed hash (hot path).
    pub fn get_pre(&self, hash: u64, key: &[u8]) -> Option<&V> {
        if self.slots.is_empty() {
            return None;
        }
        let mut i = (hash as usize) & self.mask;
        loop {
            match &self.slots[i] {
                None => return None,
                Some(e) if e.hash == hash && e.key.as_ref() == key => return Some(&e.value),
                Some(_) => i = (i + 1) & self.mask,
            }
        }
    }

    /// Mutable lookup.
    #[inline]
    pub fn get_mut(&mut self, key: &[u8]) -> Option<&mut V> {
        self.get_mut_pre(self.hash(key), key)
    }

    /// Mutable lookup using a precomputed hash (hot path).
    pub fn get_mut_pre(&mut self, hash: u64, key: &[u8]) -> Option<&mut V> {
        if self.slots.is_empty() {
            return None;
        }
        let mut i = (hash as usize) & self.mask;
        loop {
            match &self.slots[i] {
                None => return None,
                Some(e) if e.hash == hash && e.key.as_ref() == key => {
                    return self.slots[i].as_mut().map(|e| &mut e.value)
                }
                Some(_) => i = (i + 1) & self.mask,
            }
        }
    }

    #[inline]
    pub fn contains_key(&self, key: &[u8]) -> bool {
        self.get(key).is_some()
    }

    /// Insert or overwrite. Returns the previous value if the key existed.
    #[inline]
    pub fn insert(&mut self, key: &[u8], value: V) -> Option<V> {
        self.insert_pre(self.hash(key), key, value)
    }

    /// Insert/overwrite using a precomputed hash (hot path).
    pub fn insert_pre(&mut self, hash: u64, key: &[u8], value: V) -> Option<V> {
        if self.slots.is_empty() {
            self.alloc(MIN_CAP);
        } else if self.len >= self.grow_threshold() {
            self.grow();
        }

        let mut i = (hash as usize) & self.mask;
        loop {
            match &mut self.slots[i] {
                None => {
                    self.slots[i] = Some(Entry {
                        hash,
                        key: key.into(),
                        value,
                    });
                    self.len += 1;
                    return None;
                }
                Some(e) if e.hash == hash && e.key.as_ref() == key => {
                    return Some(mem::replace(&mut e.value, value));
                }
                Some(_) => i = (i + 1) & self.mask,
            }
        }
    }

    /// Remove a key, returning its value if present.
    #[inline]
    pub fn remove(&mut self, key: &[u8]) -> Option<V> {
        self.remove_pre(self.hash(key), key)
    }

    /// Remove using a precomputed hash (hot path).
    pub fn remove_pre(&mut self, hash: u64, key: &[u8]) -> Option<V> {
        if self.slots.is_empty() {
            return None;
        }
        let mut i = (hash as usize) & self.mask;
        loop {
            match &self.slots[i] {
                None => return None,
                Some(e) if e.hash == hash && e.key.as_ref() == key => break,
                Some(_) => i = (i + 1) & self.mask,
            }
        }

        let removed = self.slots[i].take().expect("located slot is occupied");
        self.len -= 1;
        self.backward_shift(i);
        Some(removed.value)
    }

    /// Restore the probe invariant after removing the entry at `hole`, by shifting later
    /// entries back into the hole where doing so does not break their own probe chains.
    fn backward_shift(&mut self, mut hole: usize) {
        let mut i = hole;
        loop {
            i = (i + 1) & self.mask;
            match &self.slots[i] {
                None => return,
                Some(e) => {
                    let ideal = (e.hash as usize) & self.mask;
                    if !cyclic_in(hole, i, ideal) {
                        self.slots[hole] = self.slots[i].take();
                        hole = i;
                    }
                }
            }
        }
    }

    fn grow(&mut self) {
        let new_cap = (self.slots.len() << 1).max(MIN_CAP);
        let old = mem::replace(&mut self.slots, (0..new_cap).map(|_| None).collect());
        self.mask = new_cap - 1;
        for slot in old.into_iter().flatten() {
            let mut i = (slot.hash as usize) & self.mask;
            while self.slots[i].is_some() {
                i = (i + 1) & self.mask;
            }
            self.slots[i] = Some(slot);
        }
    }

    /// Iterate over `(key, &value)` pairs in arbitrary order.
    pub fn iter(&self) -> impl Iterator<Item = (&[u8], &V)> {
        self.slots
            .iter()
            .filter_map(|s| s.as_ref().map(|e| (e.key.as_ref(), &e.value)))
    }
}

/// Is `x` cyclically within the half-open interval `(a, b]` on a ring of size `mask + 1`?
#[inline]
fn cyclic_in(a: usize, b: usize, x: usize) -> bool {
    if a < b {
        a < x && x <= b
    } else {
        x > a || x <= b
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn v(b: &[u8]) -> Box<[u8]> {
        b.into()
    }

    #[test]
    fn basic_insert_get_remove() {
        let mut m: FlatMap<Box<[u8]>> = FlatMap::new();
        assert_eq!(m.get(b"a"), None);
        assert_eq!(m.insert(b"a", v(b"1")), None);
        assert_eq!(m.get(b"a").map(|b| b.as_ref()), Some(&b"1"[..]));
        assert_eq!(m.len(), 1);

        assert_eq!(m.insert(b"a", v(b"2")), Some(v(b"1")));
        assert_eq!(m.get(b"a").map(|b| b.as_ref()), Some(&b"2"[..]));
        assert_eq!(m.len(), 1);

        // mutate in place
        *m.get_mut(b"a").unwrap() = v(b"3");
        assert_eq!(m.get(b"a").map(|b| b.as_ref()), Some(&b"3"[..]));

        assert_eq!(m.remove(b"a"), Some(v(b"3")));
        assert_eq!(m.get(b"a"), None);
        assert_eq!(m.len(), 0);
        assert_eq!(m.remove(b"a"), None);
    }

    #[test]
    fn grows_and_preserves_all() {
        let mut m: FlatMap<[u8; 4]> = FlatMap::new();
        for i in 0..10_000u32 {
            m.insert(&i.to_le_bytes(), i.to_be_bytes());
        }
        assert_eq!(m.len(), 10_000);
        for i in 0..10_000u32 {
            assert_eq!(m.get(&i.to_le_bytes()), Some(&i.to_be_bytes()));
        }
        assert!(m.capacity() >= 10_000);
    }

    #[test]
    fn with_capacity_avoids_early_grow() {
        let m: FlatMap<u8> = FlatMap::with_capacity(1000);
        assert!(m.capacity() >= 1000 * LOAD_DEN / LOAD_NUM);
    }

    /// Deterministic randomized cross-check against std HashMap (insert/remove/get mix).
    #[test]
    fn differential_against_hashmap() {
        let mut state: u64 = 0x9E3779B97F4A7C15;
        let mut rng = || {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state
        };

        let mut model: HashMap<Vec<u8>, Vec<u8>> = HashMap::new();
        let mut m: FlatMap<Box<[u8]>> = FlatMap::new();

        for _ in 0..200_000 {
            let key = (rng() % 2000).to_le_bytes().to_vec();
            match rng() % 3 {
                0 | 1 => {
                    let val = rng().to_le_bytes().to_vec();
                    let a = model.insert(key.clone(), val.clone());
                    let b = m.insert(&key, val.into_boxed_slice());
                    assert_eq!(a, b.map(Vec::from));
                }
                _ => {
                    let a = model.remove(&key);
                    let b = m.remove(&key);
                    assert_eq!(a, b.map(Vec::from));
                }
            }
        }

        assert_eq!(m.len(), model.len());
        for (k, val) in &model {
            assert_eq!(m.get(k).map(|b| b.as_ref()), Some(val.as_slice()));
        }
        for i in 0..2000u64 {
            let k = i.to_le_bytes();
            if !model.contains_key(&k[..]) {
                assert_eq!(m.get(&k), None);
            }
        }
    }
}
