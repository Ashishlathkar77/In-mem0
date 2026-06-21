# Benchmarks — honest status

Local dev machine (Apple Silicon, macOS). **macOS is the worst case for us** — no io_uring, so the
portable fallback is plain blocking sockets. Numbers are representative single runs, reproducible
with the scripts below. Treat as directional, not certified records.

## Multi-framework comparison (`scripts/bench-all.sh`)

Uniform driver: **`memtier_benchmark`** (so Memcached is measured on equal footing), mixed
**GET/SET 1:1**, 64-byte values, 40 connections, 50k requests/connection. Aggregate **ops/sec**
(higher is better). Versions: Redis 8.2.2, Valkey 9.1, KeyDB 6.3.4, Memcached 1.6.42.

| pipeline | **inmem** | redis | valkey | keydb | memcached |
|---|--:|--:|--:|--:|--:|
| `-P 1`  | 212,968 | **240,676** | 234,849 | 186,284 | 198,144 |
| `-P 16` | **2,153,685** 🥇 | 1,772,910 | 1,619,134 | 1,463,352 | 1,931,817 |
| `-P 64` | **4,047,772** 🥇 | 2,405,332 | 1,857,776 | 1,938,204 | 2,099,341 |

**inmem is the fastest of all systems tested at `-P 16` and `-P 64`** — at `-P 64` that's 1.68×
Redis, 2.09× KeyDB, 2.18× Valkey, 1.93× Memcached. The only case it trails is the pure
latency-bound `-P 1` (one op per round-trip), where Redis/Valkey's single epoll loop still beats
our thread-per-connection model. That gap is what the io_uring thread-per-core runtime targets.

**Dragonfly and Garnet** are Linux/.NET-only and can't run natively on this macOS box; both are
thread-per-core and very fast at high core counts. Run them via Docker with the same memtier
parameters — see [scripts/bench-docker.md](../scripts/bench-docker.md). This comparison does not
yet include them, and we don't claim to beat them until measured.

## inmem vs Redis only (`scripts/bench.sh`, `redis-benchmark`)

## Results after phase-5 hot-path optimization (v0.0.1)

Optimizations applied: **mimalloc** global allocator, **zero-copy request parsing** (arguments
read as `(offset,len)` ranges into the read buffer — no per-arg allocation), and a **borrowed
`GET` path** that encodes the stored value straight into the socket buffer (no value copy).

| Workload | inmem | Redis 7.x | Ratio |
|---|--:|--:|--:|
| SET, no pipeline (`-P 1`)  | ~191k rps  | ~234k rps  | 0.81× |
| GET, no pipeline (`-P 1`)  | ~193k rps  | ~221k rps  | 0.87× |
| SET, `-P 16`               | ~2.00M rps | ~2.04M rps | 0.98× (parity) |
| GET, `-P 16`               | ~2.52M rps | ~2.68M rps | 0.94× |
| **SET, `-P 64`**           | **~3.22M rps** | ~2.61M rps | **1.23× (we win)** |
| **GET, `-P 64`**           | **~4.95M rps** | ~4.26M rps | **1.16× (we win)** |

**We now beat Redis in the throughput-bound, high-pipelining regime** (`-P 64`): +23% on SET,
+16% on GET. This is the regime where multi-core parallelism dominates, and it validates the
core architecture — Redis is fundamentally single-threaded per shard, so once per-op overhead is
low enough, our sharded multi-threaded store scales past it.

Redis still wins the **latency-bound** `-P 1` case (one request per round-trip), because our
thread-per-connection model has higher per-op latency than Redis's single epoll loop. That gap is
exactly what the next item (io_uring thread-per-core) targets.

### Before vs after phase 5 (GET)

| | `-P 1` | `-P 16` | `-P 64` |
|---|--:|--:|--:|
| before | ~192k | ~1.77M | ~2.17M |
| after  | ~193k | ~2.52M | ~4.95M |

The high-pipelining gains (2.3× at `-P 64`) come from eliminating per-request allocation and the
`GET` value copy; the `-P 1` path is unchanged because it is latency- not allocation-bound.

## Remaining roadmap to win across the board

In leverage order (the `-P 1` / tail-latency gap is the target):

1. **io_uring thread-per-core runtime** (`glommio`/`monoio`) on Linux — removes thread-contention
   and most syscall overhead; the single biggest lever for the latency-bound case. Linux-only,
   so it can't be validated on this macOS dev box — needs a Linux benchmark run.
2. **Single-owner shards** under thread-per-core — drop the per-op shard `Mutex` entirely (keys
   route to their owning core).
3. **Pooled argv / reply buffers** — reuse the small per-command `Vec<&[u8]>` allocation.
4. **SwissTable SIMD index**, then **dashtable segments** for memory + spike-free resize.
5. Re-benchmark on Linux with coordinated-omission-aware tooling (`memtier_benchmark
   --hdr-histogram`), reporting p50/p99/p99.9.

## Reproduce

```bash
scripts/bench.sh 1000000 50   # requests, clients
```
