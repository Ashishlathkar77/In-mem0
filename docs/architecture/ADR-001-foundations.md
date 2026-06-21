# ADR-001 — Foundational Architecture

Status: Accepted · Date: 2026-06-21 · Issue: ASH-309

Backed by [research/01-landscape-and-techniques.md](../research/01-landscape-and-techniques.md).

## Context

We are building an open-source in-memory cache / key-value store that aims to beat Redis on
throughput, tail latency, and memory efficiency, while being reliable. Research identified
three decisive levers: concurrency architecture, hash-table/memory layout, and eviction policy.

## Decisions

### D1 — Language: **Rust**
- Perf parity with C++ for this workload; compile-time memory + thread safety eliminates the
  data races / use-after-free that dominate bugs in concurrent caches.
- Strong io_uring ecosystem (`glommio`, `tokio-uring`, `monoio`) and growing systems
  contributor base for open source. Chosen over C++ (manual safety) and Zig (pre-1.0, smaller pool).

### D2 — Form factor: **Embeddable core crate + RESP server shell**
- `inmem-core`: a clean, dependency-light embeddable cache library (the value).
- `inmem-server` (`inmemd` binary): a thread-per-core server speaking RESP2/3 — drop-in for
  existing Redis clients, CLI, and benchmark tools (memtier/redis-benchmark).

### D3 — Platform: **Linux-first (io_uring), portable fallback**
- Optimize for Linux io_uring + thread-per-core + NUMA/huge-pages where the tail-latency wins live.
- Provide an epoll/`tokio` portable fallback so it builds and runs on macOS/Windows for dev.

### D4 — Concurrency model: **Thread-per-core, shared-nothing**
- Each core owns a fixed set of key shards; a key is routed to its owning core. No cross-core
  locks on the hot path → near-linear core scaling (Dragonfly/Seastar pattern).
- Cross-shard/multi-key ops handled by message passing between cores (later phase).

### D5 — Index: **segmented flat (open-addressed) hash table**
- Start with a SwissTable-style open-addressing map (control bytes, SIMD probing) per shard;
  evolve toward dashtable-style **extendible hashing** with constant-size segments for
  incremental, spike-free resize and 6–16B/item overhead.
- Avoids Redis dict chaining + double-and-rehash latency spikes.

### D6 — Eviction: **lock-free FIFO (S3-FIFO), co-designed into the index**
- S3-FIFO (small/main/ghost FIFO queues) — beats LRU/ARC on miss ratio AND throughput, no
  per-hit locking. Optional W-TinyLFU admission later. Bake metadata into the table (Dragonfly
  lesson: zero per-item auxiliary pointers).

### D7 — Protocol: **RESP2/3 wire compatibility**
- Maximizes immediate usability and lets us benchmark apples-to-apples vs Redis.

### D8 — Durability (later phase): snapshot + AOF-style log
- Fork-COW or shard-local snapshotting + append-only log / WAL, designed to avoid the
  fork/resize latency spikes the flat table was chosen to eliminate.

## Workspace layout

```
Cargo.toml            # workspace
rust-toolchain.toml   # pinned stable
crates/
  core/   inmem-core  # embeddable cache: hash index + eviction + value store
  proto/  inmem-proto # RESP2/3 encode/decode
  server/ inmem-server# thread-per-core server, binary `inemd`
benches/              # criterion micro-benches; memtier/redis-benchmark scripts
```

## Build order (phased)

1. **Core map** — flat open-addressing hash table, incremental resize, unit + property tests. ← current
2. **Eviction** — S3-FIFO integrated; capacity-bounded store with metrics.
3. **Protocol + server** — RESP codec, thread-per-core runtime, GET/SET/DEL/EXPIRE.
4. **Bench harness** — coordinated-omission-correct latency; compare vs Redis.
5. **Round 2 research + build** — allocators, io_uring tuning, persistence, replication.

## Naming
`inmem` is a working name (folder is `In-mem0`); easily renamed before public release.
