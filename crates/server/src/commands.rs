//! Command dispatch: maps a parsed RESP argument vector to a [`Reply`] against the [`Store`].
//!
//! Implements the common Redis string/keyspace command surface plus the connection/introspection
//! commands (`HELLO`, `PING`, `COMMAND`, `CONFIG GET`, `CLIENT`, `INFO`) that `redis-cli`,
//! `redis-benchmark`, and `memtier_benchmark` need for handshake and setup.

use crate::config::Config;
use inmem_core::{now_ms, SetOptions, Store, Ttl};
use inmem_proto::Reply;

/// Per-connection mutable state.
pub struct ConnState {
    pub resp3: bool,
    pub name: Vec<u8>,
    pub id: u64,
}

/// The result of executing one command.
pub struct Outcome {
    pub reply: Reply,
    /// Connection should be closed after sending the reply (QUIT).
    pub quit: bool,
    /// Command mutated the keyspace and should be appended to the AOF.
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
}

fn err(msg: impl Into<String>) -> Reply {
    Reply::Error(format!("ERR {}", msg.into()))
}
fn wrong_args(cmd: &str) -> Reply {
    Reply::Error(format!("ERR wrong number of arguments for '{cmd}' command"))
}

fn eq_ignore_ascii_case(a: &[u8], b: &str) -> bool {
    a.eq_ignore_ascii_case(b.as_bytes())
}

/// Execute one command. `argv` is guaranteed non-empty by the caller.
pub fn dispatch(store: &Store, argv: &[Vec<u8>], st: &mut ConnState, cfg: &Config) -> Outcome {
    let cmd = &argv[0];
    let name = String::from_utf8_lossy(cmd).to_ascii_uppercase();

    match name.as_str() {
        "PING" => match argv.len() {
            1 => Outcome::r(Reply::Simple("PONG".into())),
            2 => Outcome::r(Reply::Bulk(Some(argv[1].clone()))),
            _ => Outcome::r(wrong_args("ping")),
        },
        "ECHO" => {
            if argv.len() != 2 {
                return Outcome::r(wrong_args("echo"));
            }
            Outcome::r(Reply::Bulk(Some(argv[1].clone())))
        }
        "HELLO" => hello(argv, st),
        "QUIT" => Outcome {
            reply: Reply::ok(),
            quit: true,
            persist: false,
        },
        "SELECT" => Outcome::r(Reply::ok()),
        "COMMAND" => command_meta(argv),
        "CONFIG" => config_cmd(argv, cfg),
        "CLIENT" => client_cmd(argv, st),
        "INFO" => Outcome::r(Reply::Bulk(Some(info_text(store, cfg).into_bytes()))),
        "DBSIZE" => Outcome::r(Reply::Int(store.dbsize() as i64)),
        "FLUSHALL" | "FLUSHDB" => {
            store.flush_all();
            Outcome::w(Reply::ok())
        }

        // ---- strings ----
        "SET" => set_cmd(store, argv),
        "GET" => {
            if argv.len() != 2 {
                return Outcome::r(wrong_args("get"));
            }
            Outcome::r(bulk_opt(store.get(&argv[1])))
        }
        "GETSET" => {
            if argv.len() != 3 {
                return Outcome::r(wrong_args("getset"));
            }
            let old = store.get(&argv[1]);
            store.set(&argv[1], &argv[2], SetOptions::default());
            Outcome::w(bulk_opt(old))
        }
        "SETNX" => {
            if argv.len() != 3 {
                return Outcome::r(wrong_args("setnx"));
            }
            let ok = store.set(
                &argv[1],
                &argv[2],
                SetOptions {
                    nx: true,
                    ..Default::default()
                },
            );
            Outcome {
                reply: Reply::Int(ok as i64),
                quit: false,
                persist: ok,
            }
        }
        "SETEX" | "PSETEX" => setex_cmd(store, argv, name == "PSETEX"),
        "MSET" => {
            if argv.len() < 3 || argv.len().is_multiple_of(2) {
                return Outcome::r(wrong_args("mset"));
            }
            for pair in argv[1..].chunks(2) {
                store.set(&pair[0], &pair[1], SetOptions::default());
            }
            Outcome::w(Reply::ok())
        }
        "MGET" => {
            if argv.len() < 2 {
                return Outcome::r(wrong_args("mget"));
            }
            let items = argv[1..].iter().map(|k| bulk_opt(store.get(k))).collect();
            Outcome::r(Reply::Array(Some(items)))
        }
        "APPEND" => {
            if argv.len() != 3 {
                return Outcome::r(wrong_args("append"));
            }
            let len = store.append(&argv[1], &argv[2]);
            Outcome::w(Reply::Int(len as i64))
        }
        "STRLEN" => {
            if argv.len() != 2 {
                return Outcome::r(wrong_args("strlen"));
            }
            Outcome::r(Reply::Int(store.strlen(&argv[1]) as i64))
        }
        "INCR" => incr_cmd(store, argv, 1, false),
        "DECR" => incr_cmd(store, argv, -1, false),
        "INCRBY" => incr_cmd(store, argv, 0, true),
        "DECRBY" => incr_cmd(store, argv, 0, true),

        // ---- keyspace ----
        "DEL" | "UNLINK" => {
            if argv.len() < 2 {
                return Outcome::r(wrong_args("del"));
            }
            let n = argv[1..].iter().filter(|k| store.del(k)).count();
            Outcome {
                reply: Reply::Int(n as i64),
                quit: false,
                persist: n > 0,
            }
        }
        "EXISTS" => {
            if argv.len() < 2 {
                return Outcome::r(wrong_args("exists"));
            }
            let n = argv[1..].iter().filter(|k| store.exists(k)).count();
            Outcome::r(Reply::Int(n as i64))
        }
        "TYPE" => {
            if argv.len() != 2 {
                return Outcome::r(wrong_args("type"));
            }
            let t = if store.exists(&argv[1]) {
                "string"
            } else {
                "none"
            };
            Outcome::r(Reply::Simple(t.into()))
        }
        "EXPIRE" => expire_cmd(store, argv, 1000, false),
        "PEXPIRE" => expire_cmd(store, argv, 1, false),
        "EXPIREAT" => expire_cmd(store, argv, 1000, true),
        "PEXPIREAT" => expire_cmd(store, argv, 1, true),
        "PERSIST" => {
            if argv.len() != 2 {
                return Outcome::r(wrong_args("persist"));
            }
            let had = matches!(store.pttl(&argv[1]), Ttl::Millis(_));
            if had {
                store.expire_at(&argv[1], None);
            }
            Outcome {
                reply: Reply::Int(had as i64),
                quit: false,
                persist: had,
            }
        }
        "TTL" => ttl_cmd(store, argv, true),
        "PTTL" => ttl_cmd(store, argv, false),
        "KEYS" => keys_cmd(store, argv),
        "SCAN" => scan_cmd(store, argv),

        // ---- persistence ----
        "SAVE" | "BGSAVE" => match crate::persistence::save_snapshot(store, &cfg.snapshot_path()) {
            Ok(_) => Outcome::r(Reply::ok()),
            Err(e) => Outcome::r(err(format!("snapshot failed: {e}"))),
        },

        _ => Outcome::r(Reply::Error(format!(
            "ERR unknown command '{}'",
            String::from_utf8_lossy(cmd)
        ))),
    }
}

fn bulk_opt(v: Option<Box<[u8]>>) -> Reply {
    match v {
        Some(b) => Reply::Bulk(Some(b.into_vec())),
        None => Reply::Bulk(None),
    }
}

fn hello(argv: &[Vec<u8>], st: &mut ConnState) -> Outcome {
    // HELLO [protover [AUTH user pass] [SETNAME name]]
    if argv.len() >= 2 {
        match std::str::from_utf8(&argv[1])
            .ok()
            .and_then(|s| s.parse::<u8>().ok())
        {
            Some(2) => st.resp3 = false,
            Some(3) => st.resp3 = true,
            _ => return Outcome::r(Reply::Error("NOPROTO unsupported protocol version".into())),
        }
    }
    let proto = if st.resp3 { 3 } else { 2 };
    let map = vec![
        (
            Reply::Bulk(Some(b"server".to_vec())),
            Reply::Bulk(Some(b"inmem".to_vec())),
        ),
        (
            Reply::Bulk(Some(b"version".to_vec())),
            Reply::Bulk(Some(b"0.0.1".to_vec())),
        ),
        (Reply::Bulk(Some(b"proto".to_vec())), Reply::Int(proto)),
        (Reply::Bulk(Some(b"id".to_vec())), Reply::Int(st.id as i64)),
        (
            Reply::Bulk(Some(b"mode".to_vec())),
            Reply::Bulk(Some(b"standalone".to_vec())),
        ),
        (
            Reply::Bulk(Some(b"role".to_vec())),
            Reply::Bulk(Some(b"master".to_vec())),
        ),
        (
            Reply::Bulk(Some(b"modules".to_vec())),
            Reply::Array(Some(vec![])),
        ),
    ];
    Outcome::r(Reply::Map(map))
}

fn command_meta(argv: &[Vec<u8>]) -> Outcome {
    // Enough for clients: COMMAND DOCS/INFO/LIST -> empty; COMMAND COUNT -> 0.
    if argv.len() >= 2 && eq_ignore_ascii_case(&argv[1], "count") {
        return Outcome::r(Reply::Int(0));
    }
    Outcome::r(Reply::Array(Some(vec![])))
}

fn config_cmd(argv: &[Vec<u8>], cfg: &Config) -> Outcome {
    if argv.len() >= 2 && eq_ignore_ascii_case(&argv[1], "get") {
        // Return [param, value] pairs for requested params we know; empty for unknown.
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
            out.push(Reply::Bulk(Some(param.clone())));
            out.push(Reply::Bulk(Some(val.into_bytes())));
        }
        return Outcome::r(Reply::Array(Some(out)));
    }
    // CONFIG SET / RESETSTAT / REWRITE -> accept silently.
    Outcome::r(Reply::ok())
}

fn client_cmd(argv: &[Vec<u8>], st: &mut ConnState) -> Outcome {
    if argv.len() < 2 {
        return Outcome::r(wrong_args("client"));
    }
    let sub = String::from_utf8_lossy(&argv[1]).to_ascii_uppercase();
    match sub.as_str() {
        "SETNAME" if argv.len() == 3 => {
            st.name = argv[2].clone();
            Outcome::r(Reply::ok())
        }
        "GETNAME" => Outcome::r(Reply::Bulk(Some(st.name.clone()))),
        "ID" => Outcome::r(Reply::Int(st.id as i64)),
        "INFO" => {
            let info = format!("id={} name={}\n", st.id, String::from_utf8_lossy(&st.name));
            Outcome::r(Reply::Bulk(Some(info.into_bytes())))
        }
        "SETINFO" | "NO-EVICT" | "NO-TOUCH" | "REPLY" => Outcome::r(Reply::ok()),
        _ => Outcome::r(Reply::ok()),
    }
}

fn info_text(store: &Store, cfg: &Config) -> String {
    format!(
        "# Server\r\nredis_version:7.4.0-inmem-0.0.1\r\ninmem_version:0.0.1\r\nmode:standalone\r\n\
         # Clients\r\nconnected_clients:1\r\n\
         # Memory\r\nmaxmemory:{}\r\nmaxmemory_policy:allkeys-lfu\r\n\
         # Keyspace\r\ndb0:keys={},expires=0,avg_ttl=0\r\n\
         # Cluster\r\ncluster_enabled:0\r\n\
         # Stats\r\nshards:{}\r\n",
        cfg.maxmemory,
        store.dbsize(),
        store.shard_count(),
    )
}

fn set_cmd(store: &Store, argv: &[Vec<u8>]) -> Outcome {
    if argv.len() < 3 {
        return Outcome::r(wrong_args("set"));
    }
    let key = &argv[1];
    let val = &argv[2];
    let mut opts = SetOptions::default();
    let mut want_get = false;
    let now = now_ms();

    let mut i = 3;
    while i < argv.len() {
        let opt = String::from_utf8_lossy(&argv[i]).to_ascii_uppercase();
        match opt.as_str() {
            "NX" => opts.nx = true,
            "XX" => opts.xx = true,
            "KEEPTTL" => opts.keep_ttl = true,
            "GET" => want_get = true,
            "EX" | "PX" | "EXAT" | "PXAT" => {
                i += 1;
                let Some(arg) = argv.get(i) else {
                    return Outcome::r(err("syntax error"));
                };
                let Some(n) = std::str::from_utf8(arg)
                    .ok()
                    .and_then(|s| s.parse::<i64>().ok())
                else {
                    return Outcome::r(err("value is not an integer or out of range"));
                };
                opts.expire_at = Some(match opt.as_str() {
                    "EX" => now + (n.max(0) as u64) * 1000,
                    "PX" => now + n.max(0) as u64,
                    "EXAT" => (n.max(0) as u64) * 1000,
                    _ => n.max(0) as u64, // PXAT
                });
            }
            _ => return Outcome::r(err("syntax error")),
        }
        i += 1;
    }

    let old = if want_get { store.get(key) } else { None };
    let did = store.set(key, val, opts);
    let reply = if want_get {
        bulk_opt(old)
    } else if did {
        Reply::ok()
    } else {
        Reply::Bulk(None) // NX/XX not satisfied
    };
    Outcome {
        reply,
        quit: false,
        persist: did,
    }
}

fn setex_cmd(store: &Store, argv: &[Vec<u8>], ms: bool) -> Outcome {
    // SETEX key seconds value
    if argv.len() != 4 {
        return Outcome::r(wrong_args(if ms { "psetex" } else { "setex" }));
    }
    let Some(n) = std::str::from_utf8(&argv[2])
        .ok()
        .and_then(|s| s.parse::<i64>().ok())
    else {
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
        &argv[1],
        &argv[3],
        SetOptions {
            expire_at,
            ..Default::default()
        },
    );
    Outcome::w(Reply::ok())
}

fn incr_cmd(store: &Store, argv: &[Vec<u8>], fixed: i64, by: bool) -> Outcome {
    let (key, delta) = if by {
        if argv.len() != 3 {
            return Outcome::r(wrong_args("incrby"));
        }
        let Some(mut d) = std::str::from_utf8(&argv[2])
            .ok()
            .and_then(|s| s.parse::<i64>().ok())
        else {
            return Outcome::r(err("value is not an integer or out of range"));
        };
        if eq_ignore_ascii_case(&argv[0], "decrby") {
            d = match d.checked_neg() {
                Some(v) => v,
                None => return Outcome::r(err("decrement would overflow")),
            };
        }
        (&argv[1], d)
    } else {
        if argv.len() != 2 {
            return Outcome::r(wrong_args("incr"));
        }
        (&argv[1], fixed)
    };
    match store.incr_by(key, delta) {
        Ok(v) => Outcome::w(Reply::Int(v)),
        Err(e) => Outcome::r(err(e)),
    }
}

fn expire_cmd(store: &Store, argv: &[Vec<u8>], unit_ms: u64, absolute: bool) -> Outcome {
    if argv.len() < 3 {
        return Outcome::r(wrong_args("expire"));
    }
    let Some(n) = std::str::from_utf8(&argv[2])
        .ok()
        .and_then(|s| s.parse::<i64>().ok())
    else {
        return Outcome::r(err("value is not an integer or out of range"));
    };
    if !store.exists(&argv[1]) {
        return Outcome::r(Reply::Int(0));
    }
    let now = now_ms();
    let new_at = if absolute {
        (n.max(0) as u64) * unit_ms
    } else {
        now + (n.max(0) as u64) * unit_ms
    };

    // Optional condition flag (NX/XX/GT/LT).
    if let Some(flag) = argv.get(3) {
        let f = String::from_utf8_lossy(flag).to_ascii_uppercase();
        let cur = store.pttl(&argv[1]);
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

    store.expire_at(&argv[1], Some(new_at));
    Outcome::w(Reply::Int(1))
}

fn ttl_cmd(store: &Store, argv: &[Vec<u8>], seconds: bool) -> Outcome {
    if argv.len() != 2 {
        return Outcome::r(wrong_args("ttl"));
    }
    let reply = match store.pttl(&argv[1]) {
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

fn keys_cmd(store: &Store, argv: &[Vec<u8>]) -> Outcome {
    if argv.len() != 2 {
        return Outcome::r(wrong_args("keys"));
    }
    let pat = &argv[1];
    let items = store
        .keys()
        .into_iter()
        .filter(|k| glob_match(pat, k))
        .map(|k| Reply::Bulk(Some(k.into_vec())))
        .collect();
    Outcome::r(Reply::Array(Some(items)))
}

fn scan_cmd(store: &Store, argv: &[Vec<u8>]) -> Outcome {
    // Minimal non-incremental SCAN: ignore cursor, return all (matched) keys with cursor 0.
    if argv.len() < 2 {
        return Outcome::r(wrong_args("scan"));
    }
    let mut pattern: Option<Vec<u8>> = None;
    let mut i = 2;
    while i < argv.len() {
        let opt = String::from_utf8_lossy(&argv[i]).to_ascii_uppercase();
        match opt.as_str() {
            "MATCH" => {
                pattern = argv.get(i + 1).cloned();
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
    fn helper(p: &[u8], s: &[u8]) -> bool {
        let mut pi = 0;
        let mut si = 0;
        let mut star: Option<(usize, usize)> = None;
        while si < s.len() {
            if pi < p.len() {
                match p[pi] {
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
                        if let Some((np, matched)) = match_class(&p[pi..], s[si]) {
                            if matched {
                                pi += np;
                                si += 1;
                                continue;
                            }
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
        while pi < p.len() && p[pi] == b'*' {
            pi += 1;
        }
        pi == p.len()
    }

    /// Returns (chars consumed in pattern, whether `c` matched the class).
    fn match_class(p: &[u8], c: u8) -> Option<(usize, bool)> {
        debug_assert_eq!(p[0], b'[');
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
            i += 1; // consume ']'
        }
        Some((i, matched ^ negate))
    }

    helper(pat, s)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn glob() {
        assert!(glob_match(b"*", b"anything"));
        assert!(glob_match(b"foo*", b"foobar"));
        assert!(glob_match(b"*bar", b"foobar"));
        assert!(glob_match(b"f?o", b"foo"));
        assert!(!glob_match(b"f?o", b"fooo"));
        assert!(glob_match(b"h[ae]llo", b"hello"));
        assert!(glob_match(b"h[ae]llo", b"hallo"));
        assert!(!glob_match(b"h[ae]llo", b"hillo"));
        assert!(glob_match(b"h[^x]llo", b"hello"));
        assert!(glob_match(b"key:[0-9]", b"key:5"));
    }

    fn cfg() -> Config {
        Config::default()
    }
    fn cs() -> ConnState {
        ConnState {
            resp3: false,
            name: Vec::new(),
            id: 1,
        }
    }
    fn a(parts: &[&[u8]]) -> Vec<Vec<u8>> {
        parts.iter().map(|p| p.to_vec()).collect()
    }

    #[test]
    fn set_get_roundtrip() {
        let st = Store::new(4, 0);
        let mut c = cs();
        let o = dispatch(&st, &a(&[b"SET", b"k", b"v"]), &mut c, &cfg());
        assert_eq!(o.reply, Reply::ok());
        assert!(o.persist);
        let o = dispatch(&st, &a(&[b"GET", b"k"]), &mut c, &cfg());
        assert_eq!(o.reply, Reply::Bulk(Some(b"v".to_vec())));
    }

    #[test]
    fn incr_and_ttl() {
        let st = Store::new(4, 0);
        let mut c = cs();
        let o = dispatch(&st, &a(&[b"INCR", b"n"]), &mut c, &cfg());
        assert_eq!(o.reply, Reply::Int(1));
        dispatch(
            &st,
            &a(&[b"SET", b"k", b"v", b"EX", b"100"]),
            &mut c,
            &cfg(),
        );
        let o = dispatch(&st, &a(&[b"TTL", b"k"]), &mut c, &cfg());
        match o.reply {
            Reply::Int(n) => assert!((90..=100).contains(&n)),
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn hello_switches_resp3() {
        let st = Store::new(2, 0);
        let mut c = cs();
        let o = dispatch(&st, &a(&[b"HELLO", b"3"]), &mut c, &cfg());
        assert!(c.resp3);
        assert!(matches!(o.reply, Reply::Map(_)));
    }

    #[test]
    fn del_exists_keys() {
        let st = Store::new(4, 0);
        let mut c = cs();
        dispatch(&st, &a(&[b"MSET", b"a", b"1", b"b", b"2"]), &mut c, &cfg());
        let o = dispatch(&st, &a(&[b"EXISTS", b"a", b"b", b"c"]), &mut c, &cfg());
        assert_eq!(o.reply, Reply::Int(2));
        let o = dispatch(&st, &a(&[b"DEL", b"a", b"c"]), &mut c, &cfg());
        assert_eq!(o.reply, Reply::Int(1));
    }
}
