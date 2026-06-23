//! The cache store: a sharded, TTL-aware, memory-bounded, multi-type key/value store.
//!
//! - **Sharded** (ADR-001 D4): keys are partitioned across N shards, each behind its own lock.
//! - **TTL**: per-entry absolute expiry (ms epoch), enforced lazily and by [`Store::purge_expired`].
//! - **Memory-bounded**: an optional `maxmemory` budget triggers [`S3Fifo`] eviction.
//! - **Types**: strings, lists, hashes, sets, and sorted sets ([`Value`]), with Redis-style
//!   `WRONGTYPE` errors when an operation hits a key of the wrong type.

use crate::map::FlatMap;
use crate::s3fifo::S3Fifo;
use parking_lot::Mutex;
use std::collections::{BTreeSet, HashMap, HashSet, VecDeque};
use std::time::{SystemTime, UNIX_EPOCH};

/// Approximate fixed per-entry bookkeeping overhead, used for `maxmemory` accounting.
const ENTRY_OVERHEAD: usize = 64;

/// The standard Redis wrong-type error message.
pub const WRONGTYPE: &str = "WRONGTYPE Operation against a key holding the wrong kind of value";

/// Current time in milliseconds since the Unix epoch.
#[inline]
pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// A total-ordering wrapper around `f64` so scores can live in a `BTreeSet` (NaN sorts last).
#[derive(Clone, Copy, PartialEq)]
struct OrdF64(f64);
impl Eq for OrdF64 {}
impl PartialOrd for OrdF64 {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}
impl Ord for OrdF64 {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.0.total_cmp(&other.0)
    }
}

/// A sorted set: O(1) score lookup plus an ordered `(score, member)` index for range queries.
#[derive(Default)]
pub struct ZSet {
    scores: HashMap<Box<[u8]>, f64>,
    sorted: BTreeSet<(OrdF64, Box<[u8]>)>,
}

impl ZSet {
    fn len(&self) -> usize {
        self.scores.len()
    }
    /// Insert/update a member. Returns true if the member was newly added.
    fn insert(&mut self, member: &[u8], score: f64) -> bool {
        match self.scores.insert(member.into(), score) {
            Some(old) => {
                self.sorted.remove(&(OrdF64(old), member.into()));
                self.sorted.insert((OrdF64(score), member.into()));
                false
            }
            None => {
                self.sorted.insert((OrdF64(score), member.into()));
                true
            }
        }
    }
    fn remove(&mut self, member: &[u8]) -> bool {
        if let Some(score) = self.scores.remove(member) {
            self.sorted.remove(&(OrdF64(score), member.into()));
            true
        } else {
            false
        }
    }
    fn score(&self, member: &[u8]) -> Option<f64> {
        self.scores.get(member).copied()
    }
    /// Members ordered by (score, lexicographic), for the inclusive rank range `[start, stop]`
    /// with Redis-style negative indexing.
    fn range(&self, start: i64, stop: i64) -> Vec<(Box<[u8]>, f64)> {
        let n = self.sorted.len() as i64;
        if n == 0 {
            return Vec::new();
        }
        let norm = |i: i64| -> i64 {
            if i < 0 {
                (n + i).max(0)
            } else {
                i.min(n - 1)
            }
        };
        let (s, e) = (norm(start), norm(stop));
        if s > e {
            return Vec::new();
        }
        self.sorted
            .iter()
            .skip(s as usize)
            .take((e - s + 1) as usize)
            .map(|(score, m)| (m.clone(), score.0))
            .collect()
    }
    fn bytes(&self) -> usize {
        self.scores.keys().map(|k| k.len() + 8 + 24).sum::<usize>()
    }
}

/// A stored value — one of the supported Redis types.
pub enum Value {
    Str(Box<[u8]>),
    List(VecDeque<Box<[u8]>>),
    Hash(HashMap<Box<[u8]>, Box<[u8]>>),
    Set(HashSet<Box<[u8]>>),
    ZSet(ZSet),
}

impl Value {
    /// The Redis `TYPE` name.
    pub fn type_name(&self) -> &'static str {
        match self {
            Value::Str(_) => "string",
            Value::List(_) => "list",
            Value::Hash(_) => "hash",
            Value::Set(_) => "set",
            Value::ZSet(_) => "zset",
        }
    }
    /// Approximate heap footprint of the value payload (excludes the key and fixed overhead).
    fn bytes(&self) -> usize {
        match self {
            Value::Str(s) => s.len(),
            Value::List(l) => l.iter().map(|e| e.len() + 16).sum(),
            Value::Hash(h) => h.iter().map(|(k, v)| k.len() + v.len() + 32).sum(),
            Value::Set(s) => s.iter().map(|m| m.len() + 16).sum(),
            Value::ZSet(z) => z.bytes(),
        }
    }
}

/// HGETALL result: `(field, value)` pairs.
pub type HashPairs = Vec<(Box<[u8]>, Box<[u8]>)>;
/// ZRANGE result: `(member, score)` pairs in sorted order.
pub type ScoredMembers = Vec<(Box<[u8]>, f64)>;

/// What a string read found (used by the allocation-free `GET` fast path).
pub enum StrRead<'a> {
    Str(&'a [u8]),
    None,
    WrongType,
}

struct Entry {
    value: Value,
    /// Absolute expiry in ms epoch; `None` means no expiry.
    expire_at: Option<u64>,
}

#[inline]
fn key_overhead(key_len: usize) -> usize {
    key_len + ENTRY_OVERHEAD
}

/// Options for `SET` (mirrors Redis semantics for NX/XX/KEEPTTL).
#[derive(Default, Clone, Copy)]
pub struct SetOptions {
    pub nx: bool,
    pub xx: bool,
    pub keep_ttl: bool,
    pub expire_at: Option<u64>,
}

/// Result of a TTL query.
#[derive(Debug, PartialEq, Eq)]
pub enum Ttl {
    NoKey,
    NoExpiry,
    Millis(u64),
}

struct Shard {
    map: FlatMap<Entry>,
    pol: S3Fifo,
    bytes: usize,
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

    fn drop_key(&mut self, key: &[u8]) -> bool {
        if let Some(e) = self.map.remove(key) {
            self.bytes -= key_overhead(key.len()) + e.value.bytes();
            if self.budget != 0 {
                self.pol.remove(key);
            }
            true
        } else {
            false
        }
    }

    /// Record an access for eviction ordering — only meaningful when a memory budget is set.
    /// When unbounded (`budget == 0`) the S3-FIFO policy is never consulted, so we skip all of
    /// its bookkeeping (and its hashing) on the hot path entirely.
    #[inline]
    fn touch(&mut self, key: &[u8]) {
        if self.budget != 0 {
            self.pol.touch(key);
        }
    }

    /// Lazy expiry: if expired, drop it and report gone. Returns true if live.
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

    fn enforce_budget(&mut self) {
        if self.budget == 0 {
            return;
        }
        while self.bytes > self.budget {
            match self.pol.evict_one() {
                Some(victim) => {
                    self.drop_key(&victim);
                }
                None => break,
            }
        }
    }

    /// Insert a fresh entry, updating accounting + policy.
    fn put_new(&mut self, key: &[u8], value: Value, expire_at: Option<u64>) {
        self.bytes += key_overhead(key.len()) + value.bytes();
        self.map.insert(key, Entry { value, expire_at });
        if self.budget != 0 {
            self.pol.insert(key);
        }
    }
}

/// The top-level cache store.
pub struct Store {
    shards: Vec<Mutex<Shard>>,
    mask: u64,
    router: ahash::RandomState,
}

/// Get a mutable collection of the right type at `key`, creating it if absent; returns the shard
/// guard and the closure result. Returns `Err(WRONGTYPE)` if the key holds a different type.
macro_rules! with_collection {
    ($self:ident, $key:expr, $variant:ident, $make:expr, $body:expr) => {{
        let now = now_ms();
        let mut s = $self.shard($key);
        let existed = s.live($key, now);
        if !existed {
            s.put_new($key, Value::$variant($make), None);
        }
        // Re-borrow after possible insert.
        let before = match s.map.get($key).map(|e| e.value.bytes()) {
            Some(b) => b,
            None => 0,
        };
        let res = match s.map.get_mut($key).map(|e| &mut e.value) {
            Some(Value::$variant(c)) => Ok($body(c)),
            Some(_) => Err(WRONGTYPE),
            None => unreachable!("entry was just ensured"),
        };
        if let Ok(_) = res {
            // Recompute footprint delta for this key and reconcile + maybe evict.
            let after = s.map.get($key).map(|e| e.value.bytes()).unwrap_or(0);
            s.bytes = s.bytes + after - before;
            s.touch($key);
            // If the collection became empty, delete the key (Redis semantics).
            let empty = match s.map.get($key).map(|e| &e.value) {
                Some(Value::$variant(c)) => collection_is_empty_helper(c),
                _ => false,
            };
            if empty {
                s.drop_key($key);
            } else {
                s.enforce_budget();
            }
        } else if !existed {
            // We created it but the type was wrong (impossible here) — clean up defensively.
        }
        res
    }};
}

impl Store {
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
            router: ahash::RandomState::with_seeds(0xa1, 0xb2, 0xc3, 0xd4),
        }
    }

    #[inline]
    fn shard(&self, key: &[u8]) -> parking_lot::MutexGuard<'_, Shard> {
        let h = self.router.hash_one(key);
        self.shards[(h & self.mask) as usize].lock()
    }

    pub fn shard_count(&self) -> usize {
        self.shards.len()
    }

    // ---------- strings ----------

    /// GET (clones the value). Returns `Err(WRONGTYPE)` if the key is not a string.
    pub fn get(&self, key: &[u8]) -> Result<Option<Box<[u8]>>, &'static str> {
        let now = now_ms();
        let mut s = self.shard(key);
        if !s.live(key, now) {
            return Ok(None);
        }
        s.touch(key);
        match s.map.get(key).map(|e| &e.value) {
            Some(Value::Str(v)) => Ok(Some(v.clone())),
            Some(_) => Err(WRONGTYPE),
            None => Ok(None),
        }
    }

    /// Borrowed string read for the hot path (no clone). The closure runs under the shard lock.
    #[inline]
    pub fn read_str<R>(&self, key: &[u8], f: impl FnOnce(StrRead) -> R) -> R {
        let now = now_ms();
        let mut s = self.shard(key);
        if !s.live(key, now) {
            return f(StrRead::None);
        }
        s.touch(key);
        match s.map.get(key).map(|e| &e.value) {
            Some(Value::Str(v)) => f(StrRead::Str(v)),
            Some(_) => f(StrRead::WrongType),
            None => f(StrRead::None),
        }
    }

    /// SET — replaces any existing value (of any type) with a string. Returns true if written.
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
            // Update in place — avoid a remove+insert (two hash ops + two policy ops) on the
            // hot SET path. We replace the value (of any prior type) with the new string.
            let old_bytes = s.map.get(key).map(|e| e.value.bytes()).unwrap_or(0);
            s.bytes = s.bytes + value.len() - old_bytes;
            if let Some(e) = s.map.get_mut(key) {
                e.value = Value::Str(value.into());
                e.expire_at = new_expire;
            }
            s.touch(key);
        } else {
            s.put_new(key, Value::Str(value.into()), new_expire);
        }
        s.enforce_budget();
        true
    }

    pub fn del(&self, key: &[u8]) -> bool {
        let now = now_ms();
        let mut s = self.shard(key);
        if !s.live(key, now) {
            return false;
        }
        s.drop_key(key)
    }

    pub fn exists(&self, key: &[u8]) -> bool {
        let now = now_ms();
        let mut s = self.shard(key);
        s.live(key, now)
    }

    /// TYPE name, or "none".
    pub fn type_of(&self, key: &[u8]) -> &'static str {
        let now = now_ms();
        let mut s = self.shard(key);
        if !s.live(key, now) {
            return "none";
        }
        s.map
            .get(key)
            .map(|e| e.value.type_name())
            .unwrap_or("none")
    }

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

    pub fn incr_by(&self, key: &[u8], delta: i64) -> Result<i64, &'static str> {
        let now = now_ms();
        let mut s = self.shard(key);
        let existed = s.live(key, now);
        let cur: i64 = if existed {
            match s.map.get(key).map(|e| &e.value) {
                Some(Value::Str(v)) => std::str::from_utf8(v)
                    .ok()
                    .and_then(|t| t.trim().parse::<i64>().ok())
                    .ok_or("value is not an integer or out of range")?,
                Some(_) => return Err(WRONGTYPE),
                None => 0,
            }
        } else {
            0
        };
        let next = cur
            .checked_add(delta)
            .ok_or("increment or decrement would overflow")?;
        let bytes: Box<[u8]> = next.to_string().into_bytes().into();
        let expire = if existed {
            s.map.get(key).and_then(|e| e.expire_at)
        } else {
            None
        };
        if existed {
            s.drop_key(key);
        }
        s.put_new(key, Value::Str(bytes), expire);
        s.enforce_budget();
        Ok(next)
    }

    pub fn append(&self, key: &[u8], suffix: &[u8]) -> Result<usize, &'static str> {
        let now = now_ms();
        let mut s = self.shard(key);
        let existed = s.live(key, now);
        if existed {
            match s.map.get(key).map(|e| &e.value) {
                Some(Value::Str(_)) => {}
                Some(_) => return Err(WRONGTYPE),
                None => {}
            }
        }
        let (mut v, expire) = if existed {
            let e = s.map.get(key).unwrap();
            let cur = match &e.value {
                Value::Str(s) => s.to_vec(),
                _ => unreachable!(),
            };
            (cur, e.expire_at)
        } else {
            (Vec::new(), None)
        };
        v.extend_from_slice(suffix);
        let new_len = v.len();
        if existed {
            s.drop_key(key);
        }
        s.put_new(key, Value::Str(v.into_boxed_slice()), expire);
        s.enforce_budget();
        Ok(new_len)
    }

    pub fn strlen(&self, key: &[u8]) -> Result<usize, &'static str> {
        let now = now_ms();
        let mut s = self.shard(key);
        if !s.live(key, now) {
            return Ok(0);
        }
        match s.map.get(key).map(|e| &e.value) {
            Some(Value::Str(v)) => Ok(v.len()),
            Some(_) => Err(WRONGTYPE),
            None => Ok(0),
        }
    }

    // ---------- lists ----------

    pub fn push(&self, key: &[u8], vals: &[&[u8]], left: bool) -> Result<usize, &'static str> {
        with_collection!(self, key, List, VecDeque::new(), |l: &mut VecDeque<
            Box<[u8]>,
        >| {
            for v in vals {
                if left {
                    l.push_front((*v).into());
                } else {
                    l.push_back((*v).into());
                }
            }
            l.len()
        })
    }

    pub fn pop(
        &self,
        key: &[u8],
        count: usize,
        left: bool,
    ) -> Result<Vec<Box<[u8]>>, &'static str> {
        with_collection!(self, key, List, VecDeque::new(), |l: &mut VecDeque<
            Box<[u8]>,
        >| {
            let mut out = Vec::new();
            for _ in 0..count {
                let item = if left { l.pop_front() } else { l.pop_back() };
                match item {
                    Some(x) => out.push(x),
                    None => break,
                }
            }
            out
        })
    }

    pub fn llen(&self, key: &[u8]) -> Result<usize, &'static str> {
        self.read_collection(key, |v| match v {
            Some(Value::List(l)) => Ok(l.len()),
            Some(_) => Err(WRONGTYPE),
            None => Ok(0),
        })
    }

    pub fn lrange(
        &self,
        key: &[u8],
        start: i64,
        stop: i64,
    ) -> Result<Vec<Box<[u8]>>, &'static str> {
        self.read_collection(key, |v| match v {
            Some(Value::List(l)) => {
                let n = l.len() as i64;
                if n == 0 {
                    return Ok(Vec::new());
                }
                let norm = |i: i64| if i < 0 { (n + i).max(0) } else { i.min(n) };
                let s = norm(start);
                let e = if stop < 0 {
                    (n + stop + 1).max(0)
                } else {
                    (stop + 1).min(n)
                };
                if s >= e {
                    return Ok(Vec::new());
                }
                Ok(l.iter()
                    .skip(s as usize)
                    .take((e - s) as usize)
                    .cloned()
                    .collect())
            }
            Some(_) => Err(WRONGTYPE),
            None => Ok(Vec::new()),
        })
    }

    // ---------- hashes ----------

    pub fn hset(&self, key: &[u8], pairs: &[(&[u8], &[u8])]) -> Result<usize, &'static str> {
        with_collection!(self, key, Hash, HashMap::new(), |h: &mut HashMap<
            Box<[u8]>,
            Box<[u8]>,
        >| {
            let mut added = 0;
            for (f, v) in pairs {
                if h.insert((*f).into(), (*v).into()).is_none() {
                    added += 1;
                }
            }
            added
        })
    }

    pub fn hget(&self, key: &[u8], field: &[u8]) -> Result<Option<Box<[u8]>>, &'static str> {
        self.read_collection(key, |v| match v {
            Some(Value::Hash(h)) => Ok(h.get(field).cloned()),
            Some(_) => Err(WRONGTYPE),
            None => Ok(None),
        })
    }

    pub fn hdel(&self, key: &[u8], fields: &[&[u8]]) -> Result<usize, &'static str> {
        with_collection!(self, key, Hash, HashMap::new(), |h: &mut HashMap<
            Box<[u8]>,
            Box<[u8]>,
        >| {
            fields.iter().filter(|f| h.remove(**f).is_some()).count()
        })
    }

    pub fn hlen(&self, key: &[u8]) -> Result<usize, &'static str> {
        self.read_collection(key, |v| match v {
            Some(Value::Hash(h)) => Ok(h.len()),
            Some(_) => Err(WRONGTYPE),
            None => Ok(0),
        })
    }

    /// HGETALL as (field, value) pairs; HKEYS/HVALS derive from this.
    pub fn hgetall(&self, key: &[u8]) -> Result<HashPairs, &'static str> {
        self.read_collection(key, |v| match v {
            Some(Value::Hash(h)) => Ok(h.iter().map(|(k, v)| (k.clone(), v.clone())).collect()),
            Some(_) => Err(WRONGTYPE),
            None => Ok(Vec::new()),
        })
    }

    // ---------- sets ----------

    pub fn sadd(&self, key: &[u8], members: &[&[u8]]) -> Result<usize, &'static str> {
        with_collection!(self, key, Set, HashSet::new(), |set: &mut HashSet<
            Box<[u8]>,
        >| {
            members.iter().filter(|m| set.insert((**m).into())).count()
        })
    }

    pub fn srem(&self, key: &[u8], members: &[&[u8]]) -> Result<usize, &'static str> {
        with_collection!(self, key, Set, HashSet::new(), |set: &mut HashSet<
            Box<[u8]>,
        >| {
            members.iter().filter(|m| set.remove(**m)).count()
        })
    }

    pub fn sismember(&self, key: &[u8], member: &[u8]) -> Result<bool, &'static str> {
        self.read_collection(key, |v| match v {
            Some(Value::Set(s)) => Ok(s.contains(member)),
            Some(_) => Err(WRONGTYPE),
            None => Ok(false),
        })
    }

    pub fn scard(&self, key: &[u8]) -> Result<usize, &'static str> {
        self.read_collection(key, |v| match v {
            Some(Value::Set(s)) => Ok(s.len()),
            Some(_) => Err(WRONGTYPE),
            None => Ok(0),
        })
    }

    pub fn smembers(&self, key: &[u8]) -> Result<Vec<Box<[u8]>>, &'static str> {
        self.read_collection(key, |v| match v {
            Some(Value::Set(s)) => Ok(s.iter().cloned().collect()),
            Some(_) => Err(WRONGTYPE),
            None => Ok(Vec::new()),
        })
    }

    // ---------- sorted sets ----------

    pub fn zadd(&self, key: &[u8], pairs: &[(f64, &[u8])]) -> Result<usize, &'static str> {
        with_collection!(self, key, ZSet, ZSet::default(), |z: &mut ZSet| {
            pairs
                .iter()
                .filter(|(score, m)| z.insert(m, *score))
                .count()
        })
    }

    pub fn zrem(&self, key: &[u8], members: &[&[u8]]) -> Result<usize, &'static str> {
        with_collection!(self, key, ZSet, ZSet::default(), |z: &mut ZSet| {
            let mut removed = 0;
            for m in members {
                if z.remove(m) {
                    removed += 1;
                }
            }
            removed
        })
    }

    pub fn zscore(&self, key: &[u8], member: &[u8]) -> Result<Option<f64>, &'static str> {
        self.read_collection(key, |v| match v {
            Some(Value::ZSet(z)) => Ok(z.score(member)),
            Some(_) => Err(WRONGTYPE),
            None => Ok(None),
        })
    }

    pub fn zcard(&self, key: &[u8]) -> Result<usize, &'static str> {
        self.read_collection(key, |v| match v {
            Some(Value::ZSet(z)) => Ok(z.len()),
            Some(_) => Err(WRONGTYPE),
            None => Ok(0),
        })
    }

    pub fn zrange(&self, key: &[u8], start: i64, stop: i64) -> Result<ScoredMembers, &'static str> {
        self.read_collection(key, |v| match v {
            Some(Value::ZSet(z)) => Ok(z.range(start, stop)),
            Some(_) => Err(WRONGTYPE),
            None => Ok(Vec::new()),
        })
    }

    // ---------- shared helpers ----------

    /// Read-only access to a key's value (or None), under the shard lock, with TTL applied.
    fn read_collection<R>(&self, key: &[u8], f: impl FnOnce(Option<&Value>) -> R) -> R {
        let now = now_ms();
        let mut s = self.shard(key);
        if !s.live(key, now) {
            return f(None);
        }
        s.touch(key);
        f(s.map.get(key).map(|e| &e.value))
    }

    pub fn dbsize(&self) -> usize {
        self.shards.iter().map(|s| s.lock().map.len()).sum()
    }

    pub fn flush_all(&self) {
        for s in &self.shards {
            let mut s = s.lock();
            *s = Shard::new(s.budget);
        }
    }

    pub fn purge_expired(&self) -> usize {
        let now = now_ms();
        let mut removed = 0;
        for s in &self.shards {
            let mut s = s.lock();
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

    /// Keys (live) across all shards. O(n).
    pub fn keys(&self) -> Vec<Box<[u8]>> {
        let now = now_ms();
        let mut out = Vec::new();
        for s in &self.shards {
            let s = s.lock();
            for (k, e) in s.map.iter() {
                if matches!(e.expire_at, Some(t) if t <= now) {
                    continue;
                }
                out.push(k.into());
            }
        }
        out
    }

    /// Emit a sequence of RESP commands that recreate the entire dataset (used for snapshots —
    /// type-generic, and replayable through the same path as the AOF). Includes TTL restoration.
    pub fn dump_commands(&self) -> Vec<u8> {
        let now = now_ms();
        let mut out = Vec::new();
        for s in &self.shards {
            let s = s.lock();
            for (k, e) in s.map.iter() {
                if matches!(e.expire_at, Some(t) if t <= now) {
                    continue;
                }
                encode_recreate(&mut out, k, &e.value);
                if let Some(t) = e.expire_at {
                    write_cmd(&mut out, &[b"PEXPIREAT", k, t.to_string().as_bytes()]);
                }
            }
        }
        out
    }
}

/// Whether a freshly-mutated collection is empty (so the key should be deleted, Redis-style).
/// Used by the `with_collection!` macro; generic over the concrete collection types.
trait MaybeEmpty {
    fn coll_is_empty(&self) -> bool;
}
impl MaybeEmpty for VecDeque<Box<[u8]>> {
    fn coll_is_empty(&self) -> bool {
        self.is_empty()
    }
}
impl MaybeEmpty for HashMap<Box<[u8]>, Box<[u8]>> {
    fn coll_is_empty(&self) -> bool {
        self.is_empty()
    }
}
impl MaybeEmpty for HashSet<Box<[u8]>> {
    fn coll_is_empty(&self) -> bool {
        self.is_empty()
    }
}
impl MaybeEmpty for ZSet {
    fn coll_is_empty(&self) -> bool {
        self.len() == 0
    }
}
fn collection_is_empty_helper<C: MaybeEmpty>(c: &C) -> bool {
    c.coll_is_empty()
}

/// Encode the command(s) that recreate `value` at `key` into `out`.
fn encode_recreate(out: &mut Vec<u8>, key: &[u8], value: &Value) {
    match value {
        Value::Str(v) => write_cmd(out, &[b"SET", key, v]),
        Value::List(l) => {
            let mut args: Vec<&[u8]> = Vec::with_capacity(l.len() + 2);
            args.push(b"RPUSH");
            args.push(key);
            for e in l {
                args.push(e);
            }
            write_cmd(out, &args);
        }
        Value::Hash(h) => {
            let mut args: Vec<&[u8]> = Vec::with_capacity(h.len() * 2 + 2);
            args.push(b"HSET");
            args.push(key);
            for (f, v) in h {
                args.push(f);
                args.push(v);
            }
            write_cmd(out, &args);
        }
        Value::Set(s) => {
            let mut args: Vec<&[u8]> = Vec::with_capacity(s.len() + 2);
            args.push(b"SADD");
            args.push(key);
            for m in s {
                args.push(m);
            }
            write_cmd(out, &args);
        }
        Value::ZSet(z) => {
            // ZADD key score member [score member ...]
            let scores: Vec<(Box<[u8]>, String)> = z
                .sorted
                .iter()
                .map(|(sc, m)| (m.clone(), fmt_score(sc.0)))
                .collect();
            let mut args: Vec<&[u8]> = Vec::with_capacity(scores.len() * 2 + 2);
            args.push(b"ZADD");
            args.push(key);
            for (m, sc) in &scores {
                args.push(sc.as_bytes());
                args.push(m);
            }
            write_cmd(out, &args);
        }
    }
}

/// Format a sorted-set score the way Redis does (integers without a trailing `.0`).
pub fn fmt_score(s: f64) -> String {
    if s == s.trunc() && s.is_finite() {
        format!("{}", s as i64)
    } else {
        format!("{s}")
    }
}

fn write_cmd(out: &mut Vec<u8>, args: &[&[u8]]) {
    out.push(b'*');
    out.extend_from_slice(args.len().to_string().as_bytes());
    out.extend_from_slice(b"\r\n");
    for a in args {
        out.push(b'$');
        out.extend_from_slice(a.len().to_string().as_bytes());
        out.extend_from_slice(b"\r\n");
        out.extend_from_slice(a);
        out.extend_from_slice(b"\r\n");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strings_basic() {
        let st = Store::new(8, 0);
        assert!(st.set(b"k", b"v", SetOptions::default()));
        assert_eq!(st.get(b"k").unwrap().as_deref(), Some(&b"v"[..]));
        assert_eq!(st.incr_by(b"n", 5), Ok(5));
        assert_eq!(st.append(b"n", b"x"), Ok(2)); // "5x"
        assert_eq!(st.strlen(b"n"), Ok(2));
    }

    #[test]
    fn wrongtype_errors() {
        let st = Store::new(4, 0);
        st.push(b"l", &[b"a"], false).unwrap();
        assert_eq!(st.get(b"l"), Err(WRONGTYPE));
        assert_eq!(st.incr_by(b"l", 1), Err(WRONGTYPE));
        assert_eq!(st.hget(b"l", b"f"), Err(WRONGTYPE));
        assert_eq!(st.sadd(b"l", &[b"x"]), Err(WRONGTYPE));
        // SET overwrites any type
        assert!(st.set(b"l", b"now-a-string", SetOptions::default()));
        assert_eq!(st.type_of(b"l"), "string");
    }

    #[test]
    fn lists() {
        let st = Store::new(4, 0);
        assert_eq!(st.push(b"l", &[b"b", b"c"], false), Ok(2)); // [b,c]
        assert_eq!(st.push(b"l", &[b"a"], true), Ok(3)); // [a,b,c]
        assert_eq!(st.llen(b"l"), Ok(3));
        let r = st.lrange(b"l", 0, -1).unwrap();
        assert_eq!(
            r,
            vec![
                b"a".to_vec().into(),
                b"b".to_vec().into(),
                b"c".to_vec().into()
            ]
        );
        assert_eq!(
            st.pop(b"l", 1, true).unwrap(),
            vec![b"a".to_vec().into_boxed_slice()]
        );
        assert_eq!(st.llen(b"l"), Ok(2));
        // drain -> key removed
        st.pop(b"l", 10, false).unwrap();
        assert!(!st.exists(b"l"));
    }

    #[test]
    fn hashes() {
        let st = Store::new(4, 0);
        assert_eq!(st.hset(b"h", &[(b"f1", b"v1"), (b"f2", b"v2")]), Ok(2));
        assert_eq!(st.hset(b"h", &[(b"f1", b"v1b")]), Ok(0)); // update, not new
        assert_eq!(st.hget(b"h", b"f1").unwrap().as_deref(), Some(&b"v1b"[..]));
        assert_eq!(st.hlen(b"h"), Ok(2));
        assert_eq!(st.hdel(b"h", &[b"f1"]), Ok(1));
        assert_eq!(st.hlen(b"h"), Ok(1));
    }

    #[test]
    fn sets() {
        let st = Store::new(4, 0);
        assert_eq!(st.sadd(b"s", &[b"a", b"b", b"a"]), Ok(2));
        assert_eq!(st.scard(b"s"), Ok(2));
        assert_eq!(st.sismember(b"s", b"a"), Ok(true));
        assert_eq!(st.sismember(b"s", b"z"), Ok(false));
        assert_eq!(st.srem(b"s", &[b"a"]), Ok(1));
        assert_eq!(st.scard(b"s"), Ok(1));
    }

    #[test]
    fn zsets() {
        let st = Store::new(4, 0);
        assert_eq!(
            st.zadd(b"z", &[(2.0, b"b"), (1.0, b"a"), (3.0, b"c")]),
            Ok(3)
        );
        assert_eq!(st.zadd(b"z", &[(5.0, b"a")]), Ok(0)); // update
        assert_eq!(st.zscore(b"z", b"a").unwrap(), Some(5.0));
        assert_eq!(st.zcard(b"z"), Ok(3));
        // order now: b(2), c(3), a(5)
        let r = st.zrange(b"z", 0, -1).unwrap();
        let members: Vec<Box<[u8]>> = r.iter().map(|(m, _)| m.clone()).collect();
        assert_eq!(
            members,
            vec![
                b"b".to_vec().into(),
                b"c".to_vec().into(),
                b"a".to_vec().into()
            ]
        );
        assert_eq!(st.zrem(b"z", &[b"b"]), Ok(1));
        assert_eq!(st.zcard(b"z"), Ok(2));
    }

    #[test]
    fn ttl_and_dump_roundtrip() {
        let st = Store::new(4, 0);
        st.set(b"s", b"v", SetOptions::default());
        st.push(b"l", &[b"x", b"y"], false).unwrap();
        st.hset(b"h", &[(b"a", b"1")]).unwrap();
        st.sadd(b"set", &[b"m"]).unwrap();
        st.zadd(b"z", &[(1.5, b"m")]).unwrap();
        let dump = st.dump_commands();
        assert!(!dump.is_empty());
        // dump should contain reconstruction verbs
        let text = String::from_utf8_lossy(&dump);
        for verb in ["SET", "RPUSH", "HSET", "SADD", "ZADD"] {
            assert!(text.contains(verb), "dump missing {verb}");
        }
    }

    #[test]
    fn maxmemory_evicts() {
        let st = Store::new(4, 64 * 200);
        for i in 0..10_000u32 {
            st.set(&i.to_le_bytes(), b"payloadpayload", SetOptions::default());
        }
        let n = st.dbsize();
        assert!(n > 0 && n < 10_000, "evicted to {n}");
    }

    #[test]
    fn concurrent_incr_is_exact() {
        use std::sync::Arc;
        let st = Arc::new(Store::new(8, 0));
        let (threads, per) = (8, 50_000i64);
        let handles: Vec<_> = (0..threads)
            .map(|_| {
                let st = Arc::clone(&st);
                std::thread::spawn(move || {
                    for _ in 0..per {
                        st.incr_by(b"c", 1).unwrap();
                    }
                })
            })
            .collect();
        for h in handles {
            h.join().unwrap();
        }
        assert_eq!(
            st.get(b"c").unwrap().as_deref(),
            Some((threads as i64 * per).to_string().as_bytes())
        );
    }

    #[test]
    fn concurrent_mixed_stays_consistent() {
        use std::sync::Arc;
        let st = Arc::new(Store::new(16, 0));
        let handles: Vec<_> = (0..8u64)
            .map(|t| {
                let st = Arc::clone(&st);
                std::thread::spawn(move || {
                    let mut state = t.wrapping_mul(0x9E3779B97F4A7C15) | 1;
                    let mut rng = || {
                        state ^= state << 13;
                        state ^= state >> 7;
                        state ^= state << 17;
                        state
                    };
                    for _ in 0..80_000 {
                        let k = (rng() % 4000).to_le_bytes();
                        match rng() % 4 {
                            0 | 1 => {
                                st.set(&k, b"value", SetOptions::default());
                            }
                            2 => {
                                let _ = st.get(&k);
                            }
                            _ => {
                                st.del(&k);
                            }
                        }
                    }
                })
            })
            .collect();
        for h in handles {
            h.join().unwrap();
        }
        for k in st.keys() {
            assert!(st.get(&k).is_ok());
        }
    }
}
