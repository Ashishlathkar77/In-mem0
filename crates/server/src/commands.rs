//! Command dispatch: maps a parsed RESP argument vector to a [`Reply`] against the [`Store`].
//!
//! Covers strings, lists, hashes, sets, and sorted sets, plus the connection/introspection
//! commands (`HELLO`, `PING`, `COMMAND`, `CONFIG GET`, `CLIENT`, `INFO`) that clients and
//! benchmark tools need. Arguments are borrowed slices (`&[&[u8]]`) — no per-arg copies.

use crate::config::Config;
use inmem_core::{fmt_score, now_ms, SetOptions, Store, Ttl, WRONGTYPE};
use inmem_proto::Reply;

/// Per-connection mutable state.
pub struct ConnState {
    pub resp3: bool,
    pub name: Vec<u8>,
    pub id: u64,
    pub authenticated: bool,
}

impl ConnState {
    pub fn new(id: u64, authenticated: bool) -> Self {
        ConnState {
            resp3: false,
            name: Vec::new(),
            id,
            authenticated,
        }
    }
    /// State for internal replay (AOF/snapshot) — pre-authenticated, no client.
    pub fn internal() -> Self {
        ConnState::new(0, true)
    }
}

/// The result of executing one command.
pub struct Outcome {
    pub reply: Reply,
    pub quit: bool,
    pub persist: bool,
}

impl Outcome {
    fn r(reply: Reply) -> Outcome {
        Outcome {
            reply,
            quit: false,
            persist: false,
        }
    }
    fn w(reply: Reply) -> Outcome {
        Outcome {
            reply,
            quit: false,
            persist: true,
        }
    }
    /// Persist only if `dirty`.
    fn maybe(reply: Reply, dirty: bool) -> Outcome {
        Outcome {
            reply,
            quit: false,
            persist: dirty,
        }
    }
}

fn err(msg: impl Into<String>) -> Reply {
    Reply::Error(format!("ERR {}", msg.into()))
}
fn wrong_args(cmd: &str) -> Reply {
    Reply::Error(format!("ERR wrong number of arguments for '{cmd}' command"))
}
/// Turn a store error string into a reply (WRONGTYPE is already a full error code; others get ERR).
fn store_err(e: &str) -> Reply {
    if e == WRONGTYPE {
        Reply::Error(e.to_string())
    } else {
        err(e)
    }
}
fn bulk(v: Option<Box<[u8]>>) -> Reply {
    match v {
        Some(b) => Reply::Bulk(Some(b.into_vec())),
        None => Reply::Bulk(None),
    }
}
fn eq_ci(a: &[u8], b: &str) -> bool {
    a.eq_ignore_ascii_case(b.as_bytes())
}
fn parse_i64(b: &[u8]) -> Option<i64> {
    std::str::from_utf8(b).ok().and_then(|s| s.parse().ok())
}
fn parse_f64(b: &[u8]) -> Option<f64> {
    std::str::from_utf8(b).ok().and_then(|s| s.parse().ok())
}

/// Execute one command. `argv` is guaranteed non-empty by the caller.
pub fn dispatch(store: &Store, argv: &[&[u8]], st: &mut ConnState, cfg: &Config) -> Outcome {
    let mut up = [0u8; 24];
    let n = argv[0].len().min(up.len());
    up[..n].copy_from_slice(&argv[0][..n]);
    up[..n].make_ascii_uppercase();
    let name = &up[..n];

    match name {
        b"PING" => match argv.len() {
            1 => Outcome::r(Reply::Simple("PONG".into())),
            2 => Outcome::r(Reply::Bulk(Some(argv[1].to_vec()))),
            _ => Outcome::r(wrong_args("ping")),
        },
        b"ECHO" if argv.len() == 2 => Outcome::r(Reply::Bulk(Some(argv[1].to_vec()))),
        b"HELLO" => hello(argv, st),
        b"QUIT" => Outcome {
            reply: Reply::ok(),
            quit: true,
            persist: false,
        },
        b"SELECT" => Outcome::r(Reply::ok()),
        b"AUTH" => auth(argv, st, cfg),
        b"COMMAND" => command_meta(argv),
        b"CONFIG" => config_cmd(argv, cfg),
        b"CLIENT" => client_cmd(argv, st),
        b"INFO" => Outcome::r(Reply::Bulk(Some(info_text(store, cfg).into_bytes()))),
        b"DBSIZE" => Outcome::r(Reply::Int(store.dbsize() as i64)),
        b"FLUSHALL" | b"FLUSHDB" => {
            store.flush_all();
            Outcome::w(Reply::ok())
        }

        // ---- strings ----
        b"SET" => set_cmd(store, argv),
        b"GET" if argv.len() == 2 => match store.get(argv[1]) {
            Ok(v) => Outcome::r(bulk(v)),
            Err(e) => Outcome::r(store_err(e)),
        },
        b"GETSET" if argv.len() == 3 => {
            let old = store.get(argv[1]);
            match old {
                Err(e) => Outcome::r(store_err(e)),
                Ok(old) => {
                    store.set(argv[1], argv[2], SetOptions::default());
                    Outcome::w(bulk(old))
                }
            }
        }
        b"SETNX" if argv.len() == 3 => {
            let ok = store.set(
                argv[1],
                argv[2],
                SetOptions {
                    nx: true,
                    ..Default::default()
                },
            );
            Outcome::maybe(Reply::Int(ok as i64), ok)
        }
        b"SETEX" | b"PSETEX" => setex_cmd(store, argv, name == b"PSETEX"),
        b"MSET" => {
            if argv.len() < 3 || argv.len().is_multiple_of(2) {
                return Outcome::r(wrong_args("mset"));
            }
            for pair in argv[1..].chunks(2) {
                store.set(pair[0], pair[1], SetOptions::default());
            }
            Outcome::w(Reply::ok())
        }
        b"MGET" if argv.len() >= 2 => {
            let items = argv[1..]
                .iter()
                .map(|k| match store.get(k) {
                    Ok(v) => bulk(v),
                    Err(_) => Reply::Bulk(None), // MGET reports wrong-type slots as nil
                })
                .collect();
            Outcome::r(Reply::Array(Some(items)))
        }
        b"APPEND" if argv.len() == 3 => match store.append(argv[1], argv[2]) {
            Ok(len) => Outcome::w(Reply::Int(len as i64)),
            Err(e) => Outcome::r(store_err(e)),
        },
        b"STRLEN" if argv.len() == 2 => match store.strlen(argv[1]) {
            Ok(len) => Outcome::r(Reply::Int(len as i64)),
            Err(e) => Outcome::r(store_err(e)),
        },
        b"INCR" => incr_cmd(store, argv, 1, false),
        b"DECR" => incr_cmd(store, argv, -1, false),
        b"INCRBY" => incr_cmd(store, argv, 0, true),
        b"DECRBY" => incr_cmd(store, argv, 0, true),

        // ---- lists ----
        b"LPUSH" => push_cmd(store, argv, true),
        b"RPUSH" => push_cmd(store, argv, false),
        b"LPOP" => pop_cmd(store, argv, true),
        b"RPOP" => pop_cmd(store, argv, false),
        b"LLEN" if argv.len() == 2 => int_or_err(store.llen(argv[1])),
        b"LRANGE" if argv.len() == 4 => lrange_cmd(store, argv),

        // ---- hashes ----
        b"HSET" | b"HMSET" => hset_cmd(store, argv, name == b"HMSET"),
        b"HGET" if argv.len() == 3 => match store.hget(argv[1], argv[2]) {
            Ok(v) => Outcome::r(bulk(v)),
            Err(e) => Outcome::r(store_err(e)),
        },
        b"HDEL" if argv.len() >= 3 => {
            let fields: Vec<&[u8]> = argv[2..].to_vec();
            match store.hdel(argv[1], &fields) {
                Ok(n) => Outcome::maybe(Reply::Int(n as i64), n > 0),
                Err(e) => Outcome::r(store_err(e)),
            }
        }
        b"HLEN" if argv.len() == 2 => int_or_err(store.hlen(argv[1])),
        b"HEXISTS" if argv.len() == 3 => match store.hget(argv[1], argv[2]) {
            Ok(v) => Outcome::r(Reply::Int(v.is_some() as i64)),
            Err(e) => Outcome::r(store_err(e)),
        },
        b"HGETALL" if argv.len() == 2 => hgetall_cmd(store, argv, st),
        b"HKEYS" | b"HVALS" if argv.len() == 2 => hkeys_vals_cmd(store, argv, name == b"HKEYS"),
        b"HMGET" if argv.len() >= 3 => hmget_cmd(store, argv),

        // ---- sets ----
        b"SADD" if argv.len() >= 3 => {
            let m: Vec<&[u8]> = argv[2..].to_vec();
            match store.sadd(argv[1], &m) {
                Ok(n) => Outcome::maybe(Reply::Int(n as i64), n > 0),
                Err(e) => Outcome::r(store_err(e)),
            }
        }
        b"SREM" if argv.len() >= 3 => {
            let m: Vec<&[u8]> = argv[2..].to_vec();
            match store.srem(argv[1], &m) {
                Ok(n) => Outcome::maybe(Reply::Int(n as i64), n > 0),
                Err(e) => Outcome::r(store_err(e)),
            }
        }
        b"SISMEMBER" if argv.len() == 3 => match store.sismember(argv[1], argv[2]) {
            Ok(b) => Outcome::r(Reply::Int(b as i64)),
            Err(e) => Outcome::r(store_err(e)),
        },
        b"SCARD" if argv.len() == 2 => int_or_err(store.scard(argv[1])),
        b"SMEMBERS" if argv.len() == 2 => match store.smembers(argv[1]) {
            Ok(m) => Outcome::r(Reply::Array(Some(
                m.into_iter()
                    .map(|x| Reply::Bulk(Some(x.into_vec())))
                    .collect(),
            ))),
            Err(e) => Outcome::r(store_err(e)),
        },

        // ---- sorted sets ----
        b"ZADD" => zadd_cmd(store, argv),
        b"ZSCORE" if argv.len() == 3 => match store.zscore(argv[1], argv[2]) {
            Ok(Some(s)) => Outcome::r(Reply::Bulk(Some(fmt_score(s).into_bytes()))),
            Ok(None) => Outcome::r(Reply::Bulk(None)),
            Err(e) => Outcome::r(store_err(e)),
        },
        b"ZREM" if argv.len() >= 3 => {
            let m: Vec<&[u8]> = argv[2..].to_vec();
            match store.zrem(argv[1], &m) {
                Ok(n) => Outcome::maybe(Reply::Int(n as i64), n > 0),
                Err(e) => Outcome::r(store_err(e)),
            }
        }
        b"ZCARD" if argv.len() == 2 => int_or_err(store.zcard(argv[1])),
        b"ZRANGE" if argv.len() >= 4 => zrange_cmd(store, argv),

        // ---- keyspace ----
        b"DEL" | b"UNLINK" if argv.len() >= 2 => {
            let n = argv[1..].iter().filter(|k| store.del(k)).count();
            Outcome::maybe(Reply::Int(n as i64), n > 0)
        }
        b"EXISTS" if argv.len() >= 2 => {
            let n = argv[1..].iter().filter(|k| store.exists(k)).count();
            Outcome::r(Reply::Int(n as i64))
        }
        b"TYPE" if argv.len() == 2 => Outcome::r(Reply::Simple(store.type_of(argv[1]).into())),
        b"EXPIRE" => expire_cmd(store, argv, 1000, false),
        b"PEXPIRE" => expire_cmd(store, argv, 1, false),
        b"EXPIREAT" => expire_cmd(store, argv, 1000, true),
        b"PEXPIREAT" => expire_cmd(store, argv, 1, true),
        b"PERSIST" if argv.len() == 2 => {
            let had = matches!(store.pttl(argv[1]), Ttl::Millis(_));
            if had {
                store.expire_at(argv[1], None);
            }
            Outcome::maybe(Reply::Int(had as i64), had)
        }
        b"TTL" => ttl_cmd(store, argv, true),
        b"PTTL" => ttl_cmd(store, argv, false),
        b"KEYS" if argv.len() == 2 => keys_cmd(store, argv),
        b"SCAN" if argv.len() >= 2 => scan_cmd(store, argv),

        // ---- persistence ----
        b"SAVE" | b"BGSAVE" => match crate::persistence::save_snapshot(store, &cfg.snapshot_path())
        {
            Ok(_) => Outcome::r(Reply::ok()),
            Err(e) => Outcome::r(err(format!("snapshot failed: {e}"))),
        },

        _ => Outcome::r(Reply::Error(format!(
            "ERR unknown command '{}'",
            String::from_utf8_lossy(argv[0])
        ))),
    }
}

fn int_or_err(r: Result<usize, &'static str>) -> Outcome {
    match r {
        Ok(n) => Outcome::r(Reply::Int(n as i64)),
        Err(e) => Outcome::r(store_err(e)),
    }
}

// ---------- connection / introspection ----------

fn auth(argv: &[&[u8]], st: &mut ConnState, cfg: &Config) -> Outcome {
    let pass = match argv.len() {
        2 => argv[1],
        3 => argv[2], // AUTH user pass — we ignore the username
        _ => return Outcome::r(wrong_args("auth")),
    };
    match &cfg.requirepass {
        None => Outcome::r(err("Client sent AUTH, but no password is set")),
        Some(want) => {
            if pass == want.as_bytes() {
                st.authenticated = true;
                Outcome::r(Reply::ok())
            } else {
                Outcome::r(Reply::Error(
                    "WRONGPASS invalid username-password pair".into(),
                ))
            }
        }
    }
}

fn hello(argv: &[&[u8]], st: &mut ConnState) -> Outcome {
    if argv.len() >= 2 {
        match parse_i64(argv[1]) {
            Some(2) => st.resp3 = false,
            Some(3) => st.resp3 = true,
            _ => return Outcome::r(Reply::Error("NOPROTO unsupported protocol version".into())),
        }
    }
    let proto = if st.resp3 { 3 } else { 2 };
    let m = |k: &[u8], v: Reply| (Reply::Bulk(Some(k.to_vec())), v);
    Outcome::r(Reply::Map(vec![
        m(b"server", Reply::Bulk(Some(b"inmem".to_vec()))),
        m(b"version", Reply::Bulk(Some(b"0.0.1".to_vec()))),
        m(b"proto", Reply::Int(proto)),
        m(b"id", Reply::Int(st.id as i64)),
        m(b"mode", Reply::Bulk(Some(b"standalone".to_vec()))),
        m(b"role", Reply::Bulk(Some(b"master".to_vec()))),
        m(b"modules", Reply::Array(Some(vec![]))),
    ]))
}

fn command_meta(argv: &[&[u8]]) -> Outcome {
    if argv.len() >= 2 && eq_ci(argv[1], "count") {
        return Outcome::r(Reply::Int(0));
    }
    Outcome::r(Reply::Array(Some(vec![])))
}

fn config_cmd(argv: &[&[u8]], cfg: &Config) -> Outcome {
    if argv.len() >= 2 && eq_ci(argv[1], "get") {
        let mut out = Vec::new();
        for param in &argv[2..] {
            let key = String::from_utf8_lossy(param).to_ascii_lowercase();
            let val = match key.as_str() {
                "maxmemory" => cfg.maxmemory.to_string(),
                "maxmemory-policy" => "allkeys-lfu".to_string(),
                "save" => String::new(),
                "appendonly" => {
                    if cfg.appendonly {
                        "yes".into()
                    } else {
                        "no".into()
                    }
                }
                _ => continue,
            };
            out.push(Reply::Bulk(Some(param.to_vec())));
            out.push(Reply::Bulk(Some(val.into_bytes())));
        }
        return Outcome::r(Reply::Array(Some(out)));
    }
    Outcome::r(Reply::ok())
}

fn client_cmd(argv: &[&[u8]], st: &mut ConnState) -> Outcome {
    if argv.len() < 2 {
        return Outcome::r(wrong_args("client"));
    }
    let sub = String::from_utf8_lossy(argv[1]).to_ascii_uppercase();
    match sub.as_str() {
        "SETNAME" if argv.len() == 3 => {
            st.name = argv[2].to_vec();
            Outcome::r(Reply::ok())
        }
        "GETNAME" => Outcome::r(Reply::Bulk(Some(st.name.clone()))),
        "ID" => Outcome::r(Reply::Int(st.id as i64)),
        "INFO" => Outcome::r(Reply::Bulk(Some(
            format!("id={} name={}\n", st.id, String::from_utf8_lossy(&st.name)).into_bytes(),
        ))),
        _ => Outcome::r(Reply::ok()),
    }
}

fn info_text(store: &Store, cfg: &Config) -> String {
    format!(
        "# Server\r\nredis_version:7.4.0-inmem-0.0.1\r\ninmem_version:0.0.1\r\nmode:standalone\r\n\
         # Clients\r\nconnected_clients:1\r\n\
         # Memory\r\nmaxmemory:{}\r\nmaxmemory_policy:allkeys-lfu\r\n\
         # Keyspace\r\ndb0:keys={},expires=0,avg_ttl=0\r\n\
         # Replication\r\nrole:master\r\n\
         # Stats\r\nshards:{}\r\n",
        cfg.maxmemory,
        store.dbsize(),
        store.shard_count(),
    )
}

// ---------- strings ----------

fn set_cmd(store: &Store, argv: &[&[u8]]) -> Outcome {
    if argv.len() < 3 {
        return Outcome::r(wrong_args("set"));
    }
    let (key, val) = (argv[1], argv[2]);
    let mut opts = SetOptions::default();
    let mut want_get = false;
    let now = now_ms();
    let mut i = 3;
    while i < argv.len() {
        let opt = String::from_utf8_lossy(argv[i]).to_ascii_uppercase();
        match opt.as_str() {
            "NX" => opts.nx = true,
            "XX" => opts.xx = true,
            "KEEPTTL" => opts.keep_ttl = true,
            "GET" => want_get = true,
            "EX" | "PX" | "EXAT" | "PXAT" => {
                i += 1;
                let Some(arg) = argv.get(i).and_then(|a| parse_i64(a)) else {
                    return Outcome::r(err("value is not an integer or out of range"));
                };
                opts.expire_at = Some(match opt.as_str() {
                    "EX" => now + (arg.max(0) as u64) * 1000,
                    "PX" => now + arg.max(0) as u64,
                    "EXAT" => (arg.max(0) as u64) * 1000,
                    _ => arg.max(0) as u64,
                });
            }
            _ => return Outcome::r(err("syntax error")),
        }
        i += 1;
    }
    let old = if want_get {
        match store.get(key) {
            Ok(v) => v,
            Err(e) => return Outcome::r(store_err(e)),
        }
    } else {
        None
    };
    let did = store.set(key, val, opts);
    let reply = if want_get {
        bulk(old)
    } else if did {
        Reply::ok()
    } else {
        Reply::Bulk(None)
    };
    Outcome::maybe(reply, did)
}

fn setex_cmd(store: &Store, argv: &[&[u8]], ms: bool) -> Outcome {
    if argv.len() != 4 {
        return Outcome::r(wrong_args(if ms { "psetex" } else { "setex" }));
    }
    let Some(n) = parse_i64(argv[2]) else {
        return Outcome::r(err("value is not an integer or out of range"));
    };
    if n <= 0 {
        return Outcome::r(err("invalid expire time"));
    }
    let now = now_ms();
    let expire_at = Some(if ms {
        now + n as u64
    } else {
        now + n as u64 * 1000
    });
    store.set(
        argv[1],
        argv[3],
        SetOptions {
            expire_at,
            ..Default::default()
        },
    );
    Outcome::w(Reply::ok())
}

fn incr_cmd(store: &Store, argv: &[&[u8]], fixed: i64, by: bool) -> Outcome {
    let (key, delta) = if by {
        if argv.len() != 3 {
            return Outcome::r(wrong_args("incrby"));
        }
        let Some(mut d) = parse_i64(argv[2]) else {
            return Outcome::r(err("value is not an integer or out of range"));
        };
        if eq_ci(argv[0], "decrby") {
            d = match d.checked_neg() {
                Some(v) => v,
                None => return Outcome::r(err("decrement would overflow")),
            };
        }
        (argv[1], d)
    } else {
        if argv.len() != 2 {
            return Outcome::r(wrong_args("incr"));
        }
        (argv[1], fixed)
    };
    match store.incr_by(key, delta) {
        Ok(v) => Outcome::w(Reply::Int(v)),
        Err(e) => Outcome::r(store_err(e)),
    }
}

// ---------- lists ----------

fn push_cmd(store: &Store, argv: &[&[u8]], left: bool) -> Outcome {
    if argv.len() < 3 {
        return Outcome::r(wrong_args(if left { "lpush" } else { "rpush" }));
    }
    let vals: Vec<&[u8]> = argv[2..].to_vec();
    match store.push(argv[1], &vals, left) {
        Ok(len) => Outcome::w(Reply::Int(len as i64)),
        Err(e) => Outcome::r(store_err(e)),
    }
}

fn pop_cmd(store: &Store, argv: &[&[u8]], left: bool) -> Outcome {
    if argv.len() != 2 && argv.len() != 3 {
        return Outcome::r(wrong_args(if left { "lpop" } else { "rpop" }));
    }
    let count = if argv.len() == 3 {
        match parse_i64(argv[2]) {
            Some(c) if c >= 0 => Some(c as usize),
            _ => return Outcome::r(err("value is out of range, must be positive")),
        }
    } else {
        None
    };
    match store.pop(argv[1], count.unwrap_or(1), left) {
        Err(e) => Outcome::r(store_err(e)),
        Ok(items) => {
            let dirty = !items.is_empty();
            let reply = if count.is_some() {
                if items.is_empty() {
                    Reply::Array(None)
                } else {
                    Reply::Array(Some(
                        items
                            .into_iter()
                            .map(|x| Reply::Bulk(Some(x.into_vec())))
                            .collect(),
                    ))
                }
            } else {
                match items.into_iter().next() {
                    Some(x) => Reply::Bulk(Some(x.into_vec())),
                    None => Reply::Bulk(None),
                }
            };
            Outcome::maybe(reply, dirty)
        }
    }
}

fn lrange_cmd(store: &Store, argv: &[&[u8]]) -> Outcome {
    let (Some(start), Some(stop)) = (parse_i64(argv[2]), parse_i64(argv[3])) else {
        return Outcome::r(err("value is not an integer or out of range"));
    };
    match store.lrange(argv[1], start, stop) {
        Ok(items) => Outcome::r(Reply::Array(Some(
            items
                .into_iter()
                .map(|x| Reply::Bulk(Some(x.into_vec())))
                .collect(),
        ))),
        Err(e) => Outcome::r(store_err(e)),
    }
}

// ---------- hashes ----------

fn hset_cmd(store: &Store, argv: &[&[u8]], hmset: bool) -> Outcome {
    if argv.len() < 4 || !argv.len().is_multiple_of(2) {
        return Outcome::r(wrong_args(if hmset { "hmset" } else { "hset" }));
    }
    let pairs: Vec<(&[u8], &[u8])> = argv[2..].chunks(2).map(|c| (c[0], c[1])).collect();
    match store.hset(argv[1], &pairs) {
        Ok(n) => {
            let reply = if hmset {
                Reply::ok()
            } else {
                Reply::Int(n as i64)
            };
            Outcome::w(reply)
        }
        Err(e) => Outcome::r(store_err(e)),
    }
}

fn hgetall_cmd(store: &Store, argv: &[&[u8]], st: &ConnState) -> Outcome {
    match store.hgetall(argv[1]) {
        Err(e) => Outcome::r(store_err(e)),
        Ok(pairs) => {
            if st.resp3 {
                Outcome::r(Reply::Map(
                    pairs
                        .into_iter()
                        .map(|(k, v)| {
                            (
                                Reply::Bulk(Some(k.into_vec())),
                                Reply::Bulk(Some(v.into_vec())),
                            )
                        })
                        .collect(),
                ))
            } else {
                let mut flat = Vec::with_capacity(pairs.len() * 2);
                for (k, v) in pairs {
                    flat.push(Reply::Bulk(Some(k.into_vec())));
                    flat.push(Reply::Bulk(Some(v.into_vec())));
                }
                Outcome::r(Reply::Array(Some(flat)))
            }
        }
    }
}

fn hkeys_vals_cmd(store: &Store, argv: &[&[u8]], keys: bool) -> Outcome {
    match store.hgetall(argv[1]) {
        Err(e) => Outcome::r(store_err(e)),
        Ok(pairs) => Outcome::r(Reply::Array(Some(
            pairs
                .into_iter()
                .map(|(k, v)| Reply::Bulk(Some(if keys { k.into_vec() } else { v.into_vec() })))
                .collect(),
        ))),
    }
}

fn hmget_cmd(store: &Store, argv: &[&[u8]]) -> Outcome {
    let items = argv[2..]
        .iter()
        .map(|f| match store.hget(argv[1], f) {
            Ok(v) => bulk(v),
            Err(_) => Reply::Bulk(None),
        })
        .collect();
    Outcome::r(Reply::Array(Some(items)))
}

// ---------- sorted sets ----------

fn zadd_cmd(store: &Store, argv: &[&[u8]]) -> Outcome {
    if argv.len() < 4 || !argv.len().is_multiple_of(2) {
        return Outcome::r(wrong_args("zadd"));
    }
    let mut pairs: Vec<(f64, &[u8])> = Vec::with_capacity((argv.len() - 2) / 2);
    for c in argv[2..].chunks(2) {
        let Some(score) = parse_f64(c[0]) else {
            return Outcome::r(err("value is not a valid float"));
        };
        pairs.push((score, c[1]));
    }
    match store.zadd(argv[1], &pairs) {
        Ok(n) => Outcome::w(Reply::Int(n as i64)),
        Err(e) => Outcome::r(store_err(e)),
    }
}

fn zrange_cmd(store: &Store, argv: &[&[u8]]) -> Outcome {
    let (Some(start), Some(stop)) = (parse_i64(argv[2]), parse_i64(argv[3])) else {
        return Outcome::r(err("value is not an integer or out of range"));
    };
    let withscores = argv.len() >= 5 && eq_ci(argv[4], "withscores");
    match store.zrange(argv[1], start, stop) {
        Err(e) => Outcome::r(store_err(e)),
        Ok(items) => {
            let mut out = Vec::new();
            for (m, sc) in items {
                out.push(Reply::Bulk(Some(m.into_vec())));
                if withscores {
                    out.push(Reply::Bulk(Some(fmt_score(sc).into_bytes())));
                }
            }
            Outcome::r(Reply::Array(Some(out)))
        }
    }
}

// ---------- keyspace ----------

fn expire_cmd(store: &Store, argv: &[&[u8]], unit_ms: u64, absolute: bool) -> Outcome {
    if argv.len() < 3 {
        return Outcome::r(wrong_args("expire"));
    }
    let Some(n) = parse_i64(argv[2]) else {
        return Outcome::r(err("value is not an integer or out of range"));
    };
    if !store.exists(argv[1]) {
        return Outcome::r(Reply::Int(0));
    }
    let now = now_ms();
    let new_at = if absolute {
        (n.max(0) as u64) * unit_ms
    } else {
        now + (n.max(0) as u64) * unit_ms
    };
    if let Some(flag) = argv.get(3) {
        let f = String::from_utf8_lossy(flag).to_ascii_uppercase();
        let cur = store.pttl(argv[1]);
        let ok = match f.as_str() {
            "NX" => matches!(cur, Ttl::NoExpiry),
            "XX" => matches!(cur, Ttl::Millis(_)),
            "GT" => matches!(cur, Ttl::Millis(ms) if new_at > now + ms),
            "LT" => {
                matches!(cur, Ttl::NoExpiry) || matches!(cur, Ttl::Millis(ms) if new_at < now + ms)
            }
            _ => return Outcome::r(err("Unsupported option")),
        };
        if !ok {
            return Outcome::r(Reply::Int(0));
        }
    }
    store.expire_at(argv[1], Some(new_at));
    Outcome::w(Reply::Int(1))
}

fn ttl_cmd(store: &Store, argv: &[&[u8]], seconds: bool) -> Outcome {
    if argv.len() != 2 {
        return Outcome::r(wrong_args("ttl"));
    }
    let reply = match store.pttl(argv[1]) {
        Ttl::NoKey => Reply::Int(-2),
        Ttl::NoExpiry => Reply::Int(-1),
        Ttl::Millis(ms) => Reply::Int(if seconds {
            ms.div_ceil(1000) as i64
        } else {
            ms as i64
        }),
    };
    Outcome::r(reply)
}

fn keys_cmd(store: &Store, argv: &[&[u8]]) -> Outcome {
    let pat = argv[1];
    let items = store
        .keys()
        .into_iter()
        .filter(|k| glob_match(pat, k))
        .map(|k| Reply::Bulk(Some(k.into_vec())))
        .collect();
    Outcome::r(Reply::Array(Some(items)))
}

fn scan_cmd(store: &Store, argv: &[&[u8]]) -> Outcome {
    let mut pattern: Option<Vec<u8>> = None;
    let mut i = 2;
    while i < argv.len() {
        let opt = String::from_utf8_lossy(argv[i]).to_ascii_uppercase();
        match opt.as_str() {
            "MATCH" => {
                pattern = argv.get(i + 1).map(|p| p.to_vec());
                i += 2;
            }
            "COUNT" | "TYPE" => i += 2,
            _ => i += 1,
        }
    }
    let items: Vec<Reply> = store
        .keys()
        .into_iter()
        .filter(|k| pattern.as_ref().map(|p| glob_match(p, k)).unwrap_or(true))
        .map(|k| Reply::Bulk(Some(k.into_vec())))
        .collect();
    Outcome::r(Reply::Array(Some(vec![
        Reply::Bulk(Some(b"0".to_vec())),
        Reply::Array(Some(items)),
    ])))
}

/// Glob matcher supporting `*`, `?`, and `[...]` classes (the Redis KEYS subset).
fn glob_match(pat: &[u8], s: &[u8]) -> bool {
    fn match_class(p: &[u8], c: u8) -> (usize, bool) {
        let mut i = 1;
        let negate = p.get(1) == Some(&b'^');
        if negate {
            i += 1;
        }
        let mut matched = false;
        while i < p.len() && p[i] != b']' {
            if i + 2 < p.len() && p[i + 1] == b'-' && p[i + 2] != b']' {
                if p[i] <= c && c <= p[i + 2] {
                    matched = true;
                }
                i += 3;
            } else {
                if p[i] == c {
                    matched = true;
                }
                i += 1;
            }
        }
        if i < p.len() {
            i += 1;
        }
        (i, matched ^ negate)
    }

    let mut pi = 0;
    let mut si = 0;
    let mut star: Option<(usize, usize)> = None;
    while si < s.len() {
        if pi < pat.len() {
            match pat[pi] {
                b'*' => {
                    star = Some((pi, si));
                    pi += 1;
                    continue;
                }
                b'?' => {
                    pi += 1;
                    si += 1;
                    continue;
                }
                b'[' => {
                    let (np, matched) = match_class(&pat[pi..], s[si]);
                    if matched {
                        pi += np;
                        si += 1;
                        continue;
                    }
                }
                c => {
                    if c == s[si] {
                        pi += 1;
                        si += 1;
                        continue;
                    }
                }
            }
        }
        if let Some((sp, ss)) = star {
            pi = sp + 1;
            si = ss + 1;
            star = Some((sp, ss + 1));
        } else {
            return false;
        }
    }
    while pi < pat.len() && pat[pi] == b'*' {
        pi += 1;
    }
    pi == pat.len()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg() -> Config {
        Config::default()
    }
    fn cs() -> ConnState {
        ConnState::new(1, true)
    }
    fn run(st_store: &Store, parts: &[&[u8]], conn: &mut ConnState) -> Reply {
        dispatch(st_store, parts, conn, &cfg()).reply
    }

    #[test]
    fn glob() {
        assert!(glob_match(b"*", b"anything"));
        assert!(glob_match(b"foo*", b"foobar"));
        assert!(glob_match(b"h[ae]llo", b"hello"));
        assert!(!glob_match(b"h[ae]llo", b"hillo"));
        assert!(glob_match(b"key:[0-9]", b"key:5"));
    }

    #[test]
    fn strings_and_keyspace() {
        let st = Store::new(4, 0);
        let mut c = cs();
        assert_eq!(run(&st, &[b"SET", b"k", b"v"], &mut c), Reply::ok());
        assert_eq!(
            run(&st, &[b"GET", b"k"], &mut c),
            Reply::Bulk(Some(b"v".to_vec()))
        );
        assert_eq!(run(&st, &[b"INCR", b"n"], &mut c), Reply::Int(1));
        assert_eq!(
            run(&st, &[b"EXISTS", b"k", b"n", b"x"], &mut c),
            Reply::Int(2)
        );
    }

    #[test]
    fn list_commands() {
        let st = Store::new(4, 0);
        let mut c = cs();
        assert_eq!(
            run(&st, &[b"RPUSH", b"l", b"a", b"b", b"c"], &mut c),
            Reply::Int(3)
        );
        assert_eq!(run(&st, &[b"LLEN", b"l"], &mut c), Reply::Int(3));
        assert_eq!(
            run(&st, &[b"LRANGE", b"l", b"0", b"-1"], &mut c),
            Reply::Array(Some(vec![
                Reply::Bulk(Some(b"a".to_vec())),
                Reply::Bulk(Some(b"b".to_vec())),
                Reply::Bulk(Some(b"c".to_vec())),
            ]))
        );
        assert_eq!(
            run(&st, &[b"LPOP", b"l"], &mut c),
            Reply::Bulk(Some(b"a".to_vec()))
        );
    }

    #[test]
    fn hash_set_zset_commands() {
        let st = Store::new(4, 0);
        let mut c = cs();
        assert_eq!(
            run(&st, &[b"HSET", b"h", b"f", b"v"], &mut c),
            Reply::Int(1)
        );
        assert_eq!(
            run(&st, &[b"HGET", b"h", b"f"], &mut c),
            Reply::Bulk(Some(b"v".to_vec()))
        );
        assert_eq!(
            run(&st, &[b"SADD", b"s", b"x", b"y", b"x"], &mut c),
            Reply::Int(2)
        );
        assert_eq!(run(&st, &[b"SISMEMBER", b"s", b"x"], &mut c), Reply::Int(1));
        assert_eq!(
            run(&st, &[b"ZADD", b"z", b"1", b"a", b"2", b"b"], &mut c),
            Reply::Int(2)
        );
        assert_eq!(
            run(&st, &[b"ZSCORE", b"z", b"b"], &mut c),
            Reply::Bulk(Some(b"2".to_vec()))
        );
        assert_eq!(
            run(&st, &[b"ZRANGE", b"z", b"0", b"-1"], &mut c),
            Reply::Array(Some(vec![
                Reply::Bulk(Some(b"a".to_vec())),
                Reply::Bulk(Some(b"b".to_vec())),
            ]))
        );
    }

    #[test]
    fn wrongtype_reply() {
        let st = Store::new(4, 0);
        let mut c = cs();
        run(&st, &[b"RPUSH", b"l", b"a"], &mut c);
        match run(&st, &[b"GET", b"l"], &mut c) {
            Reply::Error(e) => assert!(e.starts_with("WRONGTYPE")),
            other => panic!("expected WRONGTYPE, got {other:?}"),
        }
    }

    #[test]
    fn auth_required() {
        let mut c = ConnState::new(1, false);
        let config = Config {
            requirepass: Some("secret".into()),
            ..Config::default()
        };
        // wrong password
        match dispatch(&Store::new(2, 0), &[b"AUTH", b"nope"], &mut c, &config).reply {
            Reply::Error(e) => assert!(e.starts_with("WRONGPASS")),
            other => panic!("{other:?}"),
        }
        assert!(!c.authenticated);
        // right password
        let ok = dispatch(&Store::new(2, 0), &[b"AUTH", b"secret"], &mut c, &config);
        assert_eq!(ok.reply, Reply::ok());
        assert!(c.authenticated);
    }
}
