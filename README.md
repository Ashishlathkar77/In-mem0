# inmem

An open-source in-memory cache / key-value store, built in Rust, designed to beat Redis on
throughput, tail latency, and memory efficiency — while staying reliable.

> Working name. Status: **complete working v1** — a Redis-protocol-compatible server with
> eviction, TTL, and persistence. **It already beats Redis in the high-pipelining regime**
> (`-P 64`: SET +23%, GET +16%) and is at parity around `-P 16`; Redis still wins the
> latency-bound `-P 1` case (the io_uring thread-per-core work targets that). Honest numbers and
> roadmap in [docs/BENCHMARKS.md](docs/BENCHMARKS.md).
> Architecture: [docs/architecture/ADR-001-foundations.md](docs/architecture/ADR-001-foundations.md).

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

`PING ECHO HELLO QUIT SELECT COMMAND CONFIG CLIENT INFO DBSIZE FLUSHALL/FLUSHDB` ·
`SET GET GETSET SETNX SETEX PSETEX MSET MGET APPEND STRLEN INCR DECR INCRBY DECRBY` ·
`DEL UNLINK EXISTS TYPE EXPIRE PEXPIRE EXPIREAT PEXPIREAT PERSIST TTL PTTL KEYS SCAN` ·
`SAVE BGSAVE`. RESP2 and RESP3, pipelining, TTL, `maxmemory` eviction, AOF + snapshots.

## Roadmap

1. ✅ Flat open-addressing hash index (correct + fuzz-tested)
2. ✅ S3-FIFO eviction + capacity-bounded store + TTL
3. ✅ RESP2/3 codec + multithreaded server (full string/keyspace command set)
4. ✅ AOF persistence + binary snapshots + benchmark harness vs Redis
5. 🟡 **Performance** — done: mimalloc, zero-copy parsing, borrowed GET (now beats Redis at
   `-P 64`). Remaining: io_uring thread-per-core + single-owner shards (latency-bound `-P 1`),
   SwissTable-SIMD → dashtable index (see [docs/BENCHMARKS.md](docs/BENCHMARKS.md))
6. ⬜ Replication, clustering, more data types (lists/hashes/sets/sorted-sets)

## License

Apache-2.0.
