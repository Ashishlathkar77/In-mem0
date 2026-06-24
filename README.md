# inmem

An open-source in-memory cache / key-value store, built in Rust, designed to beat Redis on
throughput, tail latency, and memory efficiency — while staying reliable.

> Working name. Status: **complete working v1** — a Redis-protocol-compatible server with data
> types, eviction, TTL, persistence, AUTH, replication, and optional TLS.
> **Performance (confirmed over 3-run medians, two-machine AWS c7i.4xlarge rig, host-networked):**
> inmem is **the fastest of every system benchmarked** — it beats **Garnet** (+29% at `-P 16`, +9%
> at `-P 64`; tied at `-P 1`) and beats **Redis, Valkey, KeyDB, Memcached, and Dragonfly by 3–10×**.
> (Best inmem runtime is regime-dependent: portable wins `-P 16` at ~11.6M, io_uring wins `-P 64` at
> ~15.0M; both ship.) Honest scope: "fastest" = among **sockets/RESP cache servers** in this config
> — not vs in-process or kernel-bypass/RDMA/FPGA stores, which are a different, faster category. Full
> numbers + methodology + caveats: [docs/BENCHMARKS.md](docs/BENCHMARKS.md).
> Architecture: [docs/architecture/ADR-001-foundations.md](docs/architecture/ADR-001-foundations.md).

## Install & use (it's a drop-in Redis replacement — no Redis required)

inmem is a standalone server (`inmemd`) that speaks the Redis wire protocol (RESP2/3). You do **not**
need Redis installed — point your existing Redis client at inmem's port and it just works.

**Docker (easiest):**
```bash
docker run -d --name inmem -p 6380:6380 ghcr.io/ashishlathkar77/inmem:latest
redis-cli -p 6380 ping          # → PONG
```

**One-line install (prebuilt binary, Linux/macOS, x86_64/arm64):**
```bash
curl -fsSL https://raw.githubusercontent.com/Ashishlathkar77/In-mem0/main/install.sh | sh
inmemd --port 6380
```

**Rust users:** `cargo install inmem-server` then `inmemd --port 6380`.
**From source:** `cargo build --release && ./target/release/inmemd --port 6380`.

**Use it from your app** — same code as Redis, just the address:
```python
import redis                         # pip install redis
r = redis.Redis(host="localhost", port=6380)
r.set("user:1", "alice"); print(r.get("user:1"))
r.lpush("q", "a", "b"); r.hset("h", "f", "v"); r.zadd("z", {"m": 1.5})
```
```javascript
import Redis from "ioredis";         // npm i ioredis
const r = new Redis(6380, "localhost");
await r.set("k", "v"); console.log(await r.get("k"));
```
```go
rdb := redis.NewClient(&redis.Options{Addr: "localhost:6380"}) // go-redis
rdb.Set(ctx, "k", "v", 0)
```
Works with `redis-cli`, `redis-benchmark`, and any RESP client in any language. To **embed** inmem
in a Rust app (no server, in-process), depend on the `inmem-core` crate.

## Why / how

Design is grounded in a fact-checked survey of the fastest existing systems and the relevant
academic literature — see [docs/research/01-landscape-and-techniques.md](docs/research/01-landscape-and-techniques.md).
The three decisive levers, and our choices:

| Lever | Choice | Source of the idea |
|---|---|---|
| Concurrency | Thread-per-core, shared-nothing (lock-free hot path) | Dragonfly, Seastar/ScyllaDB |
| Hash index | Flat open-addressing → dashtable-style segments | Dragonfly dashtable, MemC3/libcuckoo, FASTER |
| Eviction | Lock-free FIFO (S3-FIFO/SIEVE), baked into the index | S3-FIFO (SOSP'23), SIEVE (NSDI'24) |
| Protocol | RESP2/3 wire-compatible (drop-in for Redis clients) | Redis, Garnet |

## Layout

```
crates/core    inmem-core   embeddable cache library (hash index + eviction)
crates/proto   inmem-proto  RESP2/3 codec
crates/server  inmem-server thread-per-core server, binary `inmemd`
docs/          architecture decision records + research
```

## Build, test, run

```bash
cargo build --release
cargo test                       # 32 tests across core/proto/server + TCP e2e

./target/release/inmemd --port 6380 --shards 8 --maxmemory 512mb
redis-cli -p 6380 ping           # works with the standard redis client
scripts/bench.sh                 # head-to-head vs redis-server
```

Requires Rust 1.96+ (pinned in `rust-toolchain.toml`).

### Supported commands

- **Connection/admin**: `PING ECHO HELLO QUIT SELECT AUTH COMMAND CONFIG CLIENT INFO DBSIZE FLUSHALL/FLUSHDB SAVE BGSAVE`
- **Strings**: `SET GET GETSET SETNX SETEX PSETEX MSET MGET APPEND STRLEN INCR DECR INCRBY DECRBY`
- **Lists**: `LPUSH RPUSH LPOP RPOP LLEN LRANGE`
- **Hashes**: `HSET HMSET HGET HMGET HDEL HLEN HEXISTS HGETALL HKEYS HVALS`
- **Sets**: `SADD SREM SISMEMBER SCARD SMEMBERS`
- **Sorted sets**: `ZADD ZSCORE ZREM ZCARD ZRANGE [WITHSCORES]`
- **Keyspace**: `DEL UNLINK EXISTS TYPE EXPIRE PEXPIRE EXPIREAT PEXPIREAT PERSIST TTL PTTL KEYS SCAN`

RESP2 + RESP3, pipelining, `WRONGTYPE` errors, TTL, `maxmemory` eviction, AOF + snapshots,
**AUTH** (`--requirepass`), **async replication** (`--replicaof`), and optional **TLS**
(`--features tls`, `--tls-cert/--tls-key`).

## Roadmap

1. ✅ Flat open-addressing hash index (correct + fuzz-tested)
2. ✅ S3-FIFO eviction + capacity-bounded store + TTL
3. ✅ RESP2/3 codec + multithreaded server (full string/keyspace command set)
4. ✅ AOF persistence + binary snapshots + benchmark harness vs Redis
5. 🟡 **Performance** — done: mimalloc, zero-copy parsing, borrowed GET; io_uring runtime built &
   benchmarked on Linux. Honest status: close 2nd to Redis on Linux, not ahead yet; io_uring v1
   underperforms the portable build. Next: give the io_uring path the fast path + single-owner
   shards (drop per-op lock), then SwissTable-SIMD → dashtable index. See
   [docs/BENCHMARKS.md](docs/BENCHMARKS.md) and [ADR-002](docs/architecture/ADR-002-io-uring-thread-per-core.md).
6. ✅ Data types (lists/hashes/sets/sorted-sets), AUTH, async replication, optional TLS
7. ⬜ Clustering, pub/sub, more commands (LINDEX/ZRANGEBYSCORE/…), Redis-compatible PSYNC

## License

Apache-2.0.
