//! S3-FIFO eviction policy (Yang et al., SOSP'23).
//!
//! Three FIFO queues — a small probationary queue `S` (~10%), a main queue `M` (~90%), and a
//! `ghost` queue `G` of recently-evicted keys — plus a tiny per-object frequency counter
//! (0..=3). It beats LRU/ARC/LIRS on miss ratio *and* multi-core throughput because hits only
//! bump a counter (no list surgery, no per-hit locking). See research/01.
//!
//! This struct owns only *ordering metadata* (keys + counters). The actual values live in the
//! [`crate::store`] shard's [`FlatMap`]. The store asks `evict_one()` for the next victim and
//! deletes it from the value map. Deletions are handled lazily: a key removed from the value
//! store is removed from `meta` here, and any stale references left in the FIFO queues are
//! skipped when they surface. (The dashtable integration in ADR-001 D6 later removes the
//! duplicate key storage entirely.)

use crate::map::FlatMap;
use std::collections::VecDeque;

/// Promote from S to M only once an object has been seen at least twice (freq > 1). This is the
/// crux of S3-FIFO: one-hit-wonders are demoted out of S quickly instead of polluting M.
const PROMOTE_THRESHOLD: u8 = 1;
const FREQ_MAX: u8 = 3;
/// Small queue target share, as a percentage of live objects.
const SMALL_PCT: usize = 10;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Loc {
    Small,
    Main,
}

#[derive(Clone, Copy)]
struct Meta {
    freq: u8,
    loc: Loc,
}

pub struct S3Fifo {
    small: VecDeque<Box<[u8]>>,
    main: VecDeque<Box<[u8]>>,
    ghost: VecDeque<Box<[u8]>>,
    /// Live ordering metadata for keys currently in the cache (S or M).
    meta: FlatMap<Meta>,
    /// Membership of the ghost queue (value byte = 1, dummy).
    ghost_set: FlatMap<()>,
    small_count: usize,
    main_count: usize,
}

impl Default for S3Fifo {
    fn default() -> Self {
        Self::new()
    }
}

impl S3Fifo {
    pub fn new() -> Self {
        S3Fifo {
            small: VecDeque::new(),
            main: VecDeque::new(),
            ghost: VecDeque::new(),
            meta: FlatMap::new(),
            ghost_set: FlatMap::new(),
            small_count: 0,
            main_count: 0,
        }
    }

    /// Number of live keys tracked (== number of values the store should be holding).
    #[inline]
    pub fn len(&self) -> usize {
        self.small_count + self.main_count
    }

    #[inline]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Record a brand-new key entering the cache. If it was recently evicted (in ghost), it is
    /// admitted straight into the main queue; otherwise it starts in the small queue.
    pub fn insert(&mut self, key: &[u8]) {
        let from_ghost = self.ghost_set.remove(key).is_some();
        if from_ghost {
            self.meta.insert(
                key,
                Meta {
                    freq: 0,
                    loc: Loc::Main,
                },
            );
            self.main.push_back(key.into());
            self.main_count += 1;
        } else {
            self.meta.insert(
                key,
                Meta {
                    freq: 0,
                    loc: Loc::Small,
                },
            );
            self.small.push_back(key.into());
            self.small_count += 1;
        }
    }

    /// Record a hit on an existing key (bump its frequency, capped).
    #[inline]
    pub fn touch(&mut self, key: &[u8]) {
        if let Some(m) = self.meta.get_mut(key) {
            m.freq = (m.freq + 1).min(FREQ_MAX);
        }
    }

    /// Notify that a key was explicitly removed from the value store (DEL/expiry).
    pub fn remove(&mut self, key: &[u8]) {
        if let Some(m) = self.meta.remove(key) {
            match m.loc {
                Loc::Small => self.small_count -= 1,
                Loc::Main => self.main_count -= 1,
            }
        }
        // Stale references left in the FIFO queues are skipped lazily in `evict_one`.
    }

    #[inline]
    pub fn contains(&self, key: &[u8]) -> bool {
        self.meta.contains_key(key)
    }

    /// Return the next key the store should evict, or `None` if the cache is empty.
    /// Promotions and stale skips happen internally; the returned key is always a real,
    /// currently-live victim that the caller must delete from its value map.
    pub fn evict_one(&mut self) -> Option<Box<[u8]>> {
        loop {
            if self.small_count == 0 && self.main_count == 0 {
                return None;
            }

            // Pick S when it exceeds its target share (and is non-empty); else M.
            let use_small = self.main_count == 0
                || (self.small_count > 0
                    && self.small_count * 100 >= (self.small_count + self.main_count) * SMALL_PCT);

            if use_small {
                if let Some(victim) = self.step_small() {
                    return Some(victim);
                }
            } else if let Some(victim) = self.step_main() {
                return Some(victim);
            }
            // otherwise: a promotion or a stale skip happened — keep going.
        }
    }

    /// One step of small-queue processing. Returns `Some(key)` if a real eviction occurred.
    fn step_small(&mut self) -> Option<Box<[u8]>> {
        let Some(k) = self.small.pop_front() else {
            self.small_count = 0; // queue drained; reconcile
            return None;
        };
        let Some(m) = self.meta.get(&k).copied() else {
            return None; // stale: key was deleted
        };
        if m.loc != Loc::Small {
            return None; // stale: already promoted
        }
        if m.freq > PROMOTE_THRESHOLD {
            // promote to main, reset frequency
            if let Some(mm) = self.meta.get_mut(&k) {
                mm.loc = Loc::Main;
                mm.freq = 0;
            }
            self.small_count -= 1;
            self.main_count += 1;
            self.main.push_back(k);
            None
        } else {
            // evict: drop value, remember in ghost so a quick re-insert lands in main
            self.meta.remove(&k);
            self.small_count -= 1;
            self.push_ghost(k.clone());
            Some(k)
        }
    }

    /// One step of main-queue processing. Returns `Some(key)` if a real eviction occurred.
    fn step_main(&mut self) -> Option<Box<[u8]>> {
        let Some(k) = self.main.pop_front() else {
            self.main_count = 0;
            return None;
        };
        let Some(m) = self.meta.get(&k).copied() else {
            return None; // stale
        };
        if m.loc != Loc::Main {
            return None; // stale
        }
        if m.freq > 0 {
            // second chance: decay and requeue
            if let Some(mm) = self.meta.get_mut(&k) {
                mm.freq -= 1;
            }
            self.main.push_back(k);
            None
        } else {
            self.meta.remove(&k);
            self.main_count -= 1;
            Some(k)
        }
    }

    fn push_ghost(&mut self, key: Box<[u8]>) {
        self.ghost_set.insert(&key, ());
        self.ghost.push_back(key);
        // Cap ghost size to roughly the live object count (a standard S3-FIFO choice).
        let cap = self.len().max(16);
        while self.ghost_set.len() > cap {
            if let Some(old) = self.ghost.pop_front() {
                self.ghost_set.remove(&old);
            } else {
                break;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    /// A count-bounded cache built on the policy, used to exercise it like the store would.
    struct CountCache {
        cap: usize,
        live: HashSet<Vec<u8>>,
        pol: S3Fifo,
    }
    impl CountCache {
        fn new(cap: usize) -> Self {
            CountCache {
                cap,
                live: HashSet::new(),
                pol: S3Fifo::new(),
            }
        }
        fn access(&mut self, key: &[u8]) -> bool {
            // returns true on hit
            if self.live.contains(key) {
                self.pol.touch(key);
                true
            } else {
                self.live.insert(key.to_vec());
                self.pol.insert(key);
                while self.pol.len() > self.cap {
                    let victim = self.pol.evict_one().expect("non-empty");
                    assert!(self.live.remove(victim.as_ref()), "victim must be live");
                }
                false
            }
        }
    }

    #[test]
    fn respects_capacity_and_consistency() {
        let mut c = CountCache::new(100);
        let mut state: u64 = 1;
        let mut rng = || {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state
        };
        for _ in 0..100_000 {
            let k = (rng() % 1000).to_le_bytes();
            c.access(&k);
            assert!(c.live.len() <= 100);
            assert_eq!(c.live.len(), c.pol.len(), "policy and value set agree");
        }
    }

    #[test]
    fn keeps_hot_keys_resident() {
        // A handful of hot keys hammered amid a flood of one-hit-wonder cold keys should
        // survive — that is exactly what S3-FIFO's small-queue filtering buys over plain FIFO.
        let mut c = CountCache::new(50);
        let hot: Vec<[u8; 8]> = (0..10u64).map(|i| i.to_le_bytes()).collect();

        // warm the hot set so they get promoted (need >1 access)
        for _ in 0..5 {
            for h in &hot {
                c.access(h);
            }
        }
        // flood with cold keys, periodically re-touching hot ones
        let mut cold: u64 = 1_000;
        for _ in 0..5_000 {
            for h in &hot {
                c.access(h);
            }
            for _ in 0..20 {
                cold += 1;
                c.access(&cold.to_le_bytes());
            }
        }
        // final pass: hot keys should overwhelmingly still be resident
        let resident = hot.iter().filter(|h| c.live.contains(&h[..])).count();
        assert!(
            resident >= 9,
            "expected >=9/10 hot keys resident, got {resident}"
        );
    }

    #[test]
    fn ghost_promotes_reinserts_to_main() {
        let mut p = S3Fifo::new();
        p.insert(b"x");
        // evict it (freq 0 -> goes to ghost)
        let v = p.evict_one().unwrap();
        assert_eq!(v.as_ref(), b"x");
        assert!(!p.contains(b"x"));
        // re-insert: should be admitted to main (was in ghost)
        p.insert(b"x");
        assert!(p.contains(b"x"));
    }
}
