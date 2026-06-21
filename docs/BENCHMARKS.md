# Benchmarks — honest status

Run with `scripts/bench.sh`. Numbers below are from a local dev machine (Apple Silicon, macOS,
`redis-benchmark`, 1M requests, 50 clients). **macOS is the worst case for us** — it has no
io_uring, so our portable fallback is plain blocking sockets. Treat these as directional.

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
