# Benchmarks — honest status

Run with `scripts/bench.sh`. Numbers below are from a local dev machine (Apple Silicon, macOS,
`redis-benchmark`, 200k–1M requests, 50 clients). **macOS is the worst case for us** — it has no
io_uring and our portable fallback is plain blocking sockets. Treat these as directional, not
final.

## Current results (v0.0.1)

| Workload | inmem | Redis 7.x | Ratio |
|---|--:|--:|--:|
| SET, no pipeline (`-P 1`) | ~186k rps | ~220k rps | 0.85× |
| GET, no pipeline (`-P 1`) | ~192k rps | ~236k rps | 0.81× |
| SET, `-P 16` | ~1.02M rps | ~2.03M rps | 0.50× |
| GET, `-P 16` | ~1.77M rps | ~2.68M rps | 0.66× |
| SET, `-P 64` | ~1.19M rps | ~2.80M rps | 0.42× |
| GET, `-P 64` | ~2.17M rps | ~4.00M rps | 0.54× |

**We are NOT yet faster than Redis.** This is the truthful baseline for a correct, complete v1.
The architecture is the right one (research-backed); the *implementation* hasn't been optimized
yet. Beating Redis is the explicit goal of the next phase, and the gap is fully explained by
known, fixable hot-path costs — not by the design.

## Why we're behind right now (root causes, each fixable)

1. **Thread-per-connection + blocking sockets.** With 50 connections we spawn 50 OS threads that
   contend for cores and do a `read`/`write` syscall per batch. Redis's single epoll loop has no
   such coordination. → Fix: Linux **io_uring thread-per-core** runtime (ADR-001 D3/D4) with a
   fixed worker = core count, each owning its connections.
2. **Per-request allocation.** Every command allocates a `Vec<Vec<u8>>` for argv, and `GET`
   copies the value twice (`Box<[u8]>` clone → `Vec` for the reply). → Fix: zero-copy parse
   borrowing from the read buffer; reply that borrows the stored bytes while the shard lock is
   held; reuse argv buffers per connection.
3. **Per-op shard mutex.** Lock/unlock on every operation. → Fix: the thread-per-core endgame
   removes the lock entirely (single owner per shard); keys route to their owning core.
4. **General-purpose allocator.** → Fix: `mimalloc`/`jemalloc` global allocator, then a slab
   allocator for entries (the dashtable integration, ADR-001 D5/D6).
5. **No SIMD probing yet.** The index is scalar linear probing. → Fix: SwissTable control-byte
   groups, then dashtable segments.

## Optimization roadmap to actually beat Redis (phase 5)

In rough order of expected leverage:

1. **io_uring thread-per-core runtime** (`glommio`/`monoio`) on Linux — the single biggest lever;
   removes thread-contention and most syscall overhead, and unlocks true multi-core scaling that
   Redis's single thread cannot match.
2. **Zero-copy request path** — parse argv as slices into the socket buffer; pooled buffers; no
   per-command heap allocation.
3. **Borrowed replies** — write `GET` values straight from the shard into the socket buffer.
4. **Global allocator swap** (mimalloc) + **slab/arena** for entries.
5. **SwissTable SIMD index**, then **dashtable segments** for memory + spike-free resize.
6. Re-benchmark on Linux with correct tail-latency methodology (coordinated-omission-aware, e.g.
   `memtier_benchmark --hdr-histogram` or `redis-benchmark` percentiles), reporting p50/p99/p99.9.

The multi-core ceiling is where we win: Redis is fundamentally single-threaded per shard, so once
the thread-per-core path and zero-copy are in, aggregate throughput should scale past it on any
multi-core box. That is the next milestone.
