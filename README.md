<div align="center">

# inmem

**A fast, Redis-compatible in-memory cache server — written in Rust.**

Drop-in for Redis (RESP2/3 wire protocol): point your existing Redis client at inmem and it just
works. No Redis required — inmem *is* the server.

[![CI](https://github.com/Ashishlathkar77/In-mem0/actions/workflows/ci.yml/badge.svg)](https://github.com/Ashishlathkar77/In-mem0/actions/workflows/ci.yml)
[![Release](https://img.shields.io/github/v/release/Ashishlathkar77/In-mem0?sort=semver)](https://github.com/Ashishlathkar77/In-mem0/releases/latest)
[![License: Apache-2.0](https://img.shields.io/badge/license-Apache--2.0-blue.svg)](LICENSE)
[![Container: GHCR](https://img.shields.io/badge/container-ghcr.io-2496ED?logo=docker&logoColor=white)](https://github.com/Ashishlathkar77/In-mem0/pkgs/container/inmem)
![Platforms](https://img.shields.io/badge/platforms-Linux%20%7C%20macOS%20%7C%20Windows-informational)

</div>

---

## Highlights

- ⚡ **Fast.** In a two-machine x86 benchmark (memtier, mixed GET/SET) inmem is the fastest of every
  system tested — ahead of **Redis, Valkey, KeyDB, Memcached, Dragonfly**, and matching/beating
  **Garnet**. See [Benchmarks](#benchmarks) (with honest caveats).
- 🔌 **Drop-in for Redis.** Speaks RESP2 & RESP3 — use your existing Redis client and tools
  (`redis-cli`, `redis-benchmark`, redis-py, ioredis, go-redis, Lettuce, …). Just change the port.
- 🧱 **Real data types.** Strings, lists, hashes, sets, sorted sets — with `WRONGTYPE` semantics.
- ⏱️ **TTL & eviction.** Per-key expiry plus a `maxmemory` budget with **S3-FIFO** eviction.
- 💾 **Durable.** Append-only file (AOF) and snapshots, replayed on startup.
- 🔁 **Replication.** Async primary → read-only replicas (`--replicaof`).
- 🔐 **Secure-ish.** Password auth (`--requirepass`) and optional TLS (`--features tls`).
- 🦀 **Safe & portable.** Pure Rust; runs on Linux, macOS, and Windows (x86_64 + arm64).
- 📦 **Embeddable.** Use the `inmem-core` crate as an in-process cache (no server, no network).

> Status: **v0.1.x — early but complete and tested** (44 tests incl. fuzz + concurrency + TCP
> e2e). API and wire compatibility are stable for the supported commands.

---

## Quick start

```bash
# Docker — works on Linux, macOS, and Windows (Docker Desktop / WSL2)
docker run -d --name inmem -p 6380:6380 ghcr.io/ashishlathkar77/inmem:latest

# Verify with the standard Redis CLI
redis-cli -p 6380 ping        # → PONG
redis-cli -p 6380 set hi there
redis-cli -p 6380 get hi      # → "there"
```

That's it — inmem is now serving on port `6380`, speaking the Redis protocol.

---

## Install

| Method | Command | Platforms |
|---|---|---|
| **Docker** | `docker run -d -p 6380:6380 ghcr.io/ashishlathkar77/inmem:latest` | Linux · macOS · Windows |
| **Script** (prebuilt binary) | `curl -fsSL https://raw.githubusercontent.com/Ashishlathkar77/In-mem0/main/install.sh \| sh` | Linux · macOS |
| **Windows binary** | Download `inmemd-windows-x86_64.exe` from [Releases](https://github.com/Ashishlathkar77/In-mem0/releases/latest), run `inmemd.exe --port 6380` | Windows |
| **Cargo** | `cargo install inmem-server` *(once published)* / or from source below | any |
| **Source** | `git clone … && cargo build --release && ./target/release/inmemd --port 6380` | any |

Prebuilt binaries are published for **linux-x86_64, linux-aarch64, macos-x86_64, macos-aarch64,
windows-x86_64** on every release.

### Docker Compose

```yaml
services:
  inmem:
    image: ghcr.io/ashishlathkar77/inmem:latest
    ports: ["6380:6380"]
    command: ["--bind", "0.0.0.0", "--port", "6380", "--maxmemory", "512mb"]
```

---

## Using inmem from your app

Use **any Redis client** — same code you'd write for Redis, just the address. (inmem does not
require Redis to be installed; it replaces it.)

<details open><summary><b>Python</b> (<code>pip install redis</code>)</summary>

```python
import redis
r = redis.Redis(host="localhost", port=6380)

r.set("user:1", "alice")
print(r.get("user:1"))                 # b'alice'
r.lpush("queue", "a", "b")             # lists
r.hset("profile", "name", "alice")     # hashes
r.sadd("tags", "x", "y")               # sets
r.zadd("board", {"alice": 10})         # sorted sets
r.expire("user:1", 60)                 # TTL
```
</details>

<details><summary><b>Node.js</b> (<code>npm i ioredis</code>)</summary>

```javascript
import Redis from "ioredis";
const r = new Redis(6380, "localhost");

await r.set("k", "v");
console.log(await r.get("k"));
await r.rpush("list", "a", "b");
await r.hset("h", "f", "v");
```
</details>

<details><summary><b>Go</b> (<code>go get github.com/redis/go-redis/v9</code>)</summary>

```go
rdb := redis.NewClient(&redis.Options{Addr: "localhost:6380"})
rdb.Set(ctx, "k", "v", 0)
val, _ := rdb.Get(ctx, "k").Result()
```
</details>

<details><summary><b>Embedded</b> (Rust, no server)</summary>

```toml
# Cargo.toml
inmem-core = "0.0"
```
```rust
use inmem_core::{Store, SetOptions};
let cache = Store::new(/*shards*/ 256, /*maxmemory bytes*/ 0);
cache.set(b"k", b"v", SetOptions::default());
assert_eq!(cache.get(b"k").unwrap().as_deref(), Some(&b"v"[..]));
```
</details>

---

## Supported commands

| Group | Commands |
|---|---|
| **Connection** | `PING` `ECHO` `HELLO` `AUTH` `SELECT` `QUIT` `COMMAND` `CONFIG GET` `CLIENT` `INFO` `DBSIZE` `FLUSHALL`/`FLUSHDB` |
| **Strings** | `SET` (`EX`/`PX`/`EXAT`/`PXAT`/`NX`/`XX`/`KEEPTTL`/`GET`) `GET` `GETSET` `SETNX` `SETEX` `PSETEX` `MSET` `MGET` `APPEND` `STRLEN` `INCR` `DECR` `INCRBY` `DECRBY` |
| **Lists** | `LPUSH` `RPUSH` `LPOP` `RPOP` `LLEN` `LRANGE` |
| **Hashes** | `HSET` `HMSET` `HGET` `HMGET` `HDEL` `HLEN` `HEXISTS` `HGETALL` `HKEYS` `HVALS` |
| **Sets** | `SADD` `SREM` `SISMEMBER` `SCARD` `SMEMBERS` |
| **Sorted sets** | `ZADD` `ZSCORE` `ZREM` `ZCARD` `ZRANGE` (`WITHSCORES`) |
| **Keyspace** | `DEL` `UNLINK` `EXISTS` `TYPE` `EXPIRE` `PEXPIRE` `EXPIREAT` `PEXPIREAT` `PERSIST` `TTL` `PTTL` `KEYS` `SCAN` |
| **Persistence** | `SAVE` `BGSAVE` |

---

## Configuration

```text
inmemd [OPTIONS]
  --port <N>             listen port (default 6380)
  --bind <ADDR>          bind address (default 127.0.0.1; use 0.0.0.0 in containers)
  --shards <N>           store partitions for concurrency (default 512)
  --maxmemory <SIZE>     memory budget, e.g. 512mb, 2gb (default: unbounded)
  --appendonly <yes|no>  enable AOF persistence (default no)
  --requirepass <PASS>   require AUTH with this password
  --replicaof <H:P>      run as a read-only replica of a primary
  --masterauth <PASS>    password to authenticate to the primary
  --tls-cert <FILE>      PEM cert chain to enable TLS (build with --features tls)
  --tls-key <FILE>       PEM private key for TLS
  --dir <PATH>           directory for persistence files (default .)
```

**Replication example:**
```bash
inmemd --port 6380                       # primary
inmemd --port 6381 --replicaof 127.0.0.1:6380   # read-only replica, auto-syncs
```

---

## Benchmarks

Two-machine AWS `c7i.4xlarge` (16 vCPU x86) rig — dedicated load generator, server over the private
network, `memtier_benchmark`, mixed GET/SET, 64-byte values. 3-run medians (ops/sec):

| System | `-P 1` | `-P 16` | `-P 64` |
|---|--:|--:|--:|
| **inmem** | **1.18M** | **11.6M** | **15.0M** |
| garnet | 1.18M | 9.0M | 13.8M |
| dragonfly | 1.07M | 3.74M | 5.73M |
| redis | 0.24M | 1.52M | 2.16M |
| valkey | 0.21M | 1.28M | 1.78M |
| keydb | 0.36M | 1.34M | 1.45M |
| memcached | 1.13M | 1.38M | 1.35M |

> **Honest scope.** "Fastest" means *among sockets/RESP cache servers in this configuration*. It is
> **not** a claim against in-process caches or kernel-bypass/RDMA/FPGA key-value stores, which are a
> different and faster category. There is ~±15% run-to-run variance, and the best inmem runtime is
> regime-dependent (portable peaks at `-P 16`, the Linux io_uring build at `-P 64`). Reproduce with
> [`scripts/bench-all.sh`](scripts/bench-all.sh). Full methodology + caveats:
> [docs/BENCHMARKS.md](docs/BENCHMARKS.md).

---

## How it works

inmem's design is grounded in a fact-checked survey of the fastest systems and the relevant
research, and validated by profiling. Key choices:

- **Sharded store**, 512 partitions, lock per shard (`parking_lot`) → low contention at high
  concurrency. The eviction policy is bypassed entirely when unbounded (`maxmemory=0`).
- **Flat open-addressing hash index** with tombstone-free backward-shift deletion.
- **S3-FIFO eviction** (SOSP'23) — beats LRU/ARC on miss ratio *and* throughput.
- **Zero-copy hot path** — borrowed `GET`, alloc-free `SET`/`INCR`, pipelined responses coalesced
  into one write; `mimalloc` allocator.
- **Optional Linux io_uring thread-per-core runtime** (`--features io-uring`).

Deep dives: [Architecture (ADR-001)](docs/architecture/ADR-001-foundations.md) ·
[io_uring runtime (ADR-002)](docs/architecture/ADR-002-io-uring-thread-per-core.md) ·
[Research notes](docs/research/) · [Benchmarks](docs/BENCHMARKS.md).

---

## Build from source

```bash
git clone https://github.com/Ashishlathkar77/In-mem0.git
cd In-mem0
cargo build --release            # portable server → target/release/inmemd
cargo test                       # 44 tests
cargo build --release --features tls         # + TLS
cargo build --release --features io-uring    # + Linux io_uring runtime
```

Requires Rust 1.96+ (pinned in `rust-toolchain.toml`).

**Project layout**

```
crates/core    inmem-core    embeddable cache: hash index, S3-FIFO eviction, sharded store
crates/proto   inmem-proto   RESP2/3 parser + encoder
crates/server  inmem-server  the inmemd server, command dispatch, persistence, replication, TLS
docs/          architecture decision records, research, benchmarks
scripts/       benchmark + provisioning helpers
```

---

## Roadmap

- [x] Strings/lists/hashes/sets/sorted-sets, TTL, eviction, persistence, AUTH, replication, TLS
- [x] Cross-platform (Linux/macOS/Windows), Docker image, prebuilt binaries
- [ ] io_uring single-owner shards (lock-free) on Linux
- [ ] More commands (`LINDEX`, `ZRANGEBYSCORE`, `GETDEL`, …) and data-type coverage
- [ ] Clustering, pub/sub, Redis-compatible `PSYNC`
- [ ] `cargo install` via crates.io, Homebrew tap

---

## Contributing

Contributions are welcome — see [CONTRIBUTING.md](CONTRIBUTING.md) (correctness first, tests
required, hot path stays allocation-free). Please also read the
[Code of Conduct](CODE_OF_CONDUCT.md). Security reports: [SECURITY.md](SECURITY.md).

## License

[Apache-2.0](LICENSE).
