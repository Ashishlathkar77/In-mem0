//! The cache store: a sharded, TTL-aware, memory-bounded key/value store.
//!
//! - **Sharded** (ADR-001 D4): keys are partitioned across N shards, each behind its own lock.
//!   Independent keys on different shards never contend. (The thread-per-core endgame replaces
//!   the lock with single-owner access; the API here is unchanged by that swap.)
//! - **TTL**: per-entry absolute expiry (ms epoch), enforced lazily on access and actively by
//!   [`Store::purge_expired`].
//! - **Memory-bounded**: an optional `maxmemory` budget triggers [`S3Fifo`] eviction.
//!
//! Values are byte strings (the Memcached/Redis-string model). Richer types (lists/hashes/sets)
//! slot in behind the same shard API later.

use crate::map::FlatMap;
use crate::s3fifo::S3Fifo;
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

/// Approximate fixed per-entry bookkeeping overhead (slot, boxed key/value headers, policy
/// metadata). Used for the `maxmemory` accounting so the budget tracks real footprint, not just
/// payload bytes.
const ENTRY_OVERHEAD: usize = 64;

/// Current time in milliseconds since the Unix epoch.
#[inline]
pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

struct Entry {
    value: Box<[u8]>,
    /// Absolute expiry in ms epoch; `None` means no expiry.
    expire_at: Option<u64>,
}

#[inline]
fn entry_bytes(key_len: usize, val_len: usize) -> usize {
    key_len + val_len + ENTRY_OVERHEAD
}

/// Options for `SET` (mirrors Redis semantics for NX/XX/KEEPTTL).
#[derive(Default, Clone, Copy)]
pub struct SetOptions {
    pub nx: bool,
    pub xx: bool,
    pub keep_ttl: bool,
    /// Absolute expiry (ms epoch) to apply, if any.
    pub expire_at: Option<u64>,
}

/// Result of a TTL query.
#[derive(Debug, PartialEq, Eq)]
pub enum Ttl {
    /// Key does not exist.
    NoKey,
    /// Key exists but has no associated expiry.
    NoExpiry,
    /// Milliseconds remaining until expiry.
    Millis(u64),
}

struct Shard {
    map: FlatMap<Entry>,
    pol: S3Fifo,
    bytes: usize,
    /// Per-shard byte budget; 0 means unbounded.
    budget: usize,
}

impl Shard {
    fn new(budget: usize) -> Self {
        Shard {
            map: FlatMap::new(),
            pol: S3Fifo::new(),
            bytes: 0,
            budget,
        }
    }

    /// Drop a key from map + policy + byte accounting. Returns true if it existed.
    fn drop_key(&mut self, key: &[u8]) -> bool {
        if let Some(e) = self.map.remove(key) {
            self.bytes -= entry_bytes(key.len(), e.value.len());
            self.pol.remove(key);
            true
        } else {
            false
        }
    }

    /// Lazy expiry check: if expired, drop it and report as gone. Returns true if live.
    fn live(&mut self, key: &[u8], now: u64) -> bool {
        match self.map.get(key) {
            None => false,
            Some(e) => match e.expire_at {
                Some(t) if t <= now => {
                    self.drop_key(key);
                    false
                }
                _ => true,
            },
        }
    }

    /// Evict until back under budget (if any). No-op when unbounded.
    fn enforce_budget(&mut self) {
        if self.budget == 0 {
            return;
        }
        while self.bytes > self.budget {
            match self.pol.evict_one() {
                Some(victim) => {
                    self.drop_key(&victim);
                }
                None => break, // nothing left to evict
            }
        }
    }
}

/// The top-level cache store.
pub struct Store {
    shards: Vec<Mutex<Shard>>,
    mask: u64,
    router: ahash::RandomState,
}

impl Store {
    /// Create a store with `shards` shards (rounded up to a power of two) and an overall
    /// `maxmemory` byte budget (0 = unbounded), split evenly across shards.
    pub fn new(shards: usize, maxmemory: usize) -> Self {
        let n = shards.max(1).next_power_of_two();
        let per_shard = if maxmemory == 0 {
            0
        } else {
            (maxmemory / n).max(1)
        };
        Store {
            shards: (0..n).map(|_| Mutex::new(Shard::new(per_shard))).collect(),
            mask: (n as u64) - 1,
            // Fixed seeds so shard routing is stable across the process (not security-sensitive).
            router: ahash::RandomState::with_seeds(0xa1, 0xb2, 0xc3, 0xd4),
        }
    }

    #[inline]
    fn shard(&self, key: &[u8]) -> std::sync::MutexGuard<'_, Shard> {
        let h = self.router.hash_one(key);
        self.shards[(h & self.mask) as usize].lock().unwrap()
    }

    /// Number of shards.
    pub fn shard_count(&self) -> usize {
        self.shards.len()
    }

    /// GET — returns a copy of the value, or None (also lazily expires).
    pub fn get(&self, key: &[u8]) -> Option<Box<[u8]>> {
        let now = now_ms();
        let mut s = self.shard(key);
        if !s.live(key, now) {
            return None;
        }
        s.pol.touch(key);
        s.map.get(key).map(|e| e.value.clone())
    }

    /// SET with options. Returns true if the value was written.
    pub fn set(&self, key: &[u8], value: &[u8], opts: SetOptions) -> bool {
        let now = now_ms();
        let mut s = self.shard(key);
        let existed = s.live(key, now);

        if opts.nx && existed {
            return false;
        }
        if opts.xx && !existed {
            return false;
        }

        let new_expire = if opts.keep_ttl && existed {
            s.map.get(key).and_then(|e| e.expire_at)
        } else {
            opts.expire_at
        };

        if existed {
            // overwrite: adjust byte accounting by value-size delta
            let old_len = s.map.get(key).map(|e| e.value.len()).unwrap_or(0);
            s.bytes = s.bytes - old_len + value.len();
            if let Some(e) = s.map.get_mut(key) {
                e.value = value.into();
                e.expire_at = new_expire;
            }
            s.pol.touch(key);
        } else {
            s.bytes += entry_bytes(key.len(), value.len());
            s.map.insert(
                key,
                Entry {
                    value: value.into(),
                    expire_at: new_expire,
                },
            );
            s.pol.insert(key);
        }
        s.enforce_budget();
        true
    }

    /// DEL one key. Returns true if it existed.
    pub fn del(&self, key: &[u8]) -> bool {
        let now = now_ms();
        let mut s = self.shard(key);
        if !s.live(key, now) {
            return false;
        }
        s.drop_key(key)
    }

    /// EXISTS.
    pub fn exists(&self, key: &[u8]) -> bool {
        let now = now_ms();
        let mut s = self.shard(key);
        s.live(key, now)
    }

    /// Set/clear absolute expiry on an existing key. Returns true if the key existed.
    pub fn expire_at(&self, key: &[u8], at: Option<u64>) -> bool {
        let now = now_ms();
        let mut s = self.shard(key);
        if !s.live(key, now) {
            return false;
        }
        if let Some(e) = s.map.get_mut(key) {
            e.expire_at = at;
            true
        } else {
            false
        }
    }

    /// PTTL — milliseconds remaining, or a no-key / no-expiry marker.
    pub fn pttl(&self, key: &[u8]) -> Ttl {
        let now = now_ms();
        let mut s = self.shard(key);
        if !s.live(key, now) {
            return Ttl::NoKey;
        }
        match s.map.get(key).and_then(|e| e.expire_at) {
            None => Ttl::NoExpiry,
            Some(t) => Ttl::Millis(t.saturating_sub(now)),
        }
    }

    /// INCRBY / DECRBY. Parses the current value as i64 (missing == 0), applies `delta`, stores
    /// the decimal result. Errors on non-integer values and overflow.
    pub fn incr_by(&self, key: &[u8], delta: i64) -> Result<i64, &'static str> {
        let now = now_ms();
        let mut s = self.shard(key);
        let existed = s.live(key, now);

        let cur: i64 = if existed {
            let v = s.map.get(key).unwrap().value.as_ref();
            std::str::from_utf8(v)
                .ok()
                .and_then(|t| t.trim().parse::<i64>().ok())
                .ok_or("value is not an integer or out of range")?
        } else {
            0
        };

        let next = cur
            .checked_add(delta)
            .ok_or("increment or decrement would overflow")?;
        let text = next.to_string();
        let bytes = text.as_bytes();

        if existed {
            let old_len = s.map.get(key).map(|e| e.value.len()).unwrap_or(0);
            s.bytes = s.bytes - old_len + bytes.len();
            if let Some(e) = s.map.get_mut(key) {
                e.value = bytes.into();
                // INCR preserves TTL
            }
            s.pol.touch(key);
        } else {
            s.bytes += entry_bytes(key.len(), bytes.len());
            s.map.insert(
                key,
                Entry {
                    value: bytes.into(),
                    expire_at: None,
                },
            );
            s.pol.insert(key);
        }
        s.enforce_budget();
        Ok(next)
    }

    /// APPEND — append to (or create) a string, returning the new length.
    pub fn append(&self, key: &[u8], suffix: &[u8]) -> usize {
        let now = now_ms();
        let mut s = self.shard(key);
        let existed = s.live(key, now);
        if existed {
            let mut v = s.map.get(key).unwrap().value.to_vec();
            v.extend_from_slice(suffix);
            let new_len = v.len();
            let old_len = s.map.get(key).unwrap().value.len();
            s.bytes = s.bytes - old_len + new_len;
            if let Some(e) = s.map.get_mut(key) {
                e.value = v.into_boxed_slice();
            }
            s.pol.touch(key);
            s.enforce_budget();
            new_len
        } else {
            s.bytes += entry_bytes(key.len(), suffix.len());
            s.map.insert(
                key,
                Entry {
                    value: suffix.into(),
                    expire_at: None,
                },
            );
            s.pol.insert(key);
            s.enforce_budget();
            suffix.len()
        }
    }

    /// STRLEN.
    pub fn strlen(&self, key: &[u8]) -> usize {
        let now = now_ms();
        let mut s = self.shard(key);
        if !s.live(key, now) {
            return 0;
        }
        s.map.get(key).map(|e| e.value.len()).unwrap_or(0)
    }

    /// Total number of live (non-expired) keys across all shards.
    pub fn dbsize(&self) -> usize {
        self.shards
            .iter()
            .map(|s| s.lock().unwrap().pol.len())
            .sum()
    }

    /// Remove everything.
    pub fn flush_all(&self) {
        for s in &self.shards {
            let mut s = s.lock().unwrap();
            *s = Shard::new(s.budget);
        }
    }

    /// Active expiry sweep: drop expired keys. Returns how many were removed. Intended to be
    /// called periodically by a background reaper.
    pub fn purge_expired(&self) -> usize {
        let now = now_ms();
        let mut removed = 0;
        for s in &self.shards {
            let mut s = s.lock().unwrap();
            let expired: Vec<Box<[u8]>> = s
                .map
                .iter()
                .filter(|(_, e)| matches!(e.expire_at, Some(t) if t <= now))
                .map(|(k, _)| k.into())
                .collect();
            for k in expired {
                if s.drop_key(&k) {
                    removed += 1;
                }
            }
        }
        removed
    }

    /// All live (non-expired) keys. O(n); used by `KEYS`/`SCAN`. Order is unspecified.
    pub fn keys(&self) -> Vec<Box<[u8]>> {
        let now = now_ms();
        let mut out = Vec::new();
        for s in &self.shards {
            let s = s.lock().unwrap();
            for (k, e) in s.map.iter() {
                if matches!(e.expire_at, Some(t) if t <= now) {
                    continue;
                }
                out.push(k.into());
            }
        }
        out
    }

    /// Snapshot every live key as `(key, value, expire_at)`. Used by persistence (snapshot/AOF
    /// rewrite). O(n); call off the hot path.
    pub fn snapshot(&self) -> Vec<(Box<[u8]>, Box<[u8]>, Option<u64>)> {
        let now = now_ms();
        let mut out = Vec::new();
        for s in &self.shards {
            let s = s.lock().unwrap();
            for (k, e) in s.map.iter() {
                if matches!(e.expire_at, Some(t) if t <= now) {
                    continue;
                }
                out.push((k.into(), e.value.clone(), e.expire_at));
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn set_get_del() {
        let st = Store::new(8, 0);
        assert!(st.set(b"k", b"v", SetOptions::default()));
        assert_eq!(st.get(b"k").as_deref(), Some(&b"v"[..]));
        assert!(st.exists(b"k"));
        assert!(st.del(b"k"));
        assert!(!st.del(b"k"));
        assert_eq!(st.get(b"k"), None);
    }

    #[test]
    fn nx_xx() {
        let st = Store::new(4, 0);
        let nx = SetOptions {
            nx: true,
            ..Default::default()
        };
        let xx = SetOptions {
            xx: true,
            ..Default::default()
        };
        assert!(st.set(b"k", b"1", nx)); // not present -> ok
        assert!(!st.set(b"k", b"2", nx)); // present -> refused
        assert_eq!(st.get(b"k").as_deref(), Some(&b"1"[..]));
        assert!(st.set(b"k", b"3", xx)); // present -> ok
        assert_eq!(st.get(b"k").as_deref(), Some(&b"3"[..]));
        assert!(!st.set(b"missing", b"x", xx)); // absent -> refused
    }

    #[test]
    fn ttl_expiry() {
        let st = Store::new(4, 0);
        let past = SetOptions {
            expire_at: Some(now_ms().saturating_sub(1)),
            ..Default::default()
        };
        st.set(b"dead", b"v", past);
        assert_eq!(st.get(b"dead"), None); // lazily expired
        assert_eq!(st.pttl(b"dead"), Ttl::NoKey);

        let future = SetOptions {
            expire_at: Some(now_ms() + 100_000),
            ..Default::default()
        };
        st.set(b"live", b"v", future);
        assert!(matches!(st.pttl(b"live"), Ttl::Millis(_)));

        st.set(b"perm", b"v", SetOptions::default());
        assert_eq!(st.pttl(b"perm"), Ttl::NoExpiry);
    }

    #[test]
    fn incr_and_append() {
        let st = Store::new(4, 0);
        assert_eq!(st.incr_by(b"n", 1), Ok(1));
        assert_eq!(st.incr_by(b"n", 10), Ok(11));
        assert_eq!(st.incr_by(b"n", -5), Ok(6));
        st.set(b"s", b"foo", SetOptions::default());
        assert!(st.incr_by(b"s", 1).is_err());
        assert_eq!(st.append(b"s", b"bar"), 6);
        assert_eq!(st.get(b"s").as_deref(), Some(&b"foobar"[..]));
        assert_eq!(st.strlen(b"s"), 6);
    }

    #[test]
    fn maxmemory_evicts_and_keeps_under_budget() {
        // tiny budget; insert far more than fits -> store must stay bounded and never panic
        let st = Store::new(4, 64 * 200); // ~200 small entries worth, spread over 4 shards
        for i in 0..10_000u32 {
            let k = i.to_le_bytes();
            st.set(&k, b"payloadpayload", SetOptions::default());
        }
        let n = st.dbsize();
        assert!(n > 0, "should retain some keys");
        assert!(n < 10_000, "should have evicted, got {n}");
    }

    #[test]
    fn dbsize_flush_snapshot() {
        let st = Store::new(4, 0);
        for i in 0..100u32 {
            st.set(&i.to_le_bytes(), b"v", SetOptions::default());
        }
        assert_eq!(st.dbsize(), 100);
        assert_eq!(st.snapshot().len(), 100);
        st.flush_all();
        assert_eq!(st.dbsize(), 0);
    }
}
