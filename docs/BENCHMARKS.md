# Benchmarks — honest status

Driver: `memtier_benchmark`, mixed **GET/SET 1:1**, 64-byte values. Numbers are representative
single runs and vary ±10–15%. **Results depend heavily on cores and connection count** — inmem is
multi-threaded (scales with both); Redis/Valkey are single-threaded (cap out). Both data points
below are real and reported in full; the x86 server run is the production-representative one.

---

## Primary: AWS EC2 c7i.4xlarge — 16 vCPU x86, Ubuntu, kernel 6.17

100 connections (memtier `-t 4 -c 25`), 100k req/conn. **Docker competitors run with
`--network host`**, so there is no NAT penalty — this is a fair cross-compare with the native rows.

| system | net | `-P 1` | `-P 16` | `-P 64` |
|---|---|--:|--:|--:|
| **inmem (portable)** | native | **426k** | **3.17M** | 4.68M |
| garnet               | docker | 419k | 3.03M | **5.08M** |
| inmem (io_uring v1)  | native | 339k | 2.55M | 4.04M |
| dragonfly            | docker | 331k | 1.88M | 2.85M |
| keydb                | docker | 318k | 1.34M | 1.40M |
| redis 7.x            | native | 208k | 1.59M | 2.22M |
| memcached            | native | 445k | 1.65M | 1.64M |
| valkey               | docker | 194k | 1.33M | 1.84M |

### Conclusions (x86, 16 cores, 100 connections)
- **inmem-portable beats Redis (~2×), Valkey, KeyDB, Dragonfly, and Memcached** at every pipeline
  level that involves real batching. This is the multi-core thread-per-shard design paying off where
  it should: many cores + many connections.
- **Garnet is the strongest competitor** — it ties inmem at `-P 1`/`-P 16` and wins at `-P 64`
  (5.08M vs 4.68M). Microsoft's thread-per-core .NET store is the bar to beat at extreme pipelining.
- **Memcached** has the best single-op `-P 1` (445k) but doesn't scale with pipelining.
- **The io_uring runtime (v1) is still ~10–20% slower than the portable build** — it routes through
  the allocating `dispatch` path and shares the `Mutex` store, so it lacks the zero-copy fast path
  and lock-free shards. Fixing that (below) should make it the fastest config.

---

## Secondary: ARM lima VM — 8 vCPU, 40 connections (lower concurrency)

Here Redis *led* (P1 467k / P16 2.93M / P64 4.11M) and inmem-portable was a close 2nd
(291k / 2.65M / 3.84M). The flip vs the x86 run is explained by concurrency: at only 8 cores and
40 connections, single-threaded Redis keeps up; at 16 cores / 100 connections it's the bottleneck.
**Takeaway: inmem's lead widens with cores and connections, and narrows (or reverses) at low
concurrency.** Don't read either run as the whole story — state the conditions.

(macOS runs showed inmem leading at high pipelining too, but macOS is not a production target and
Redis's kqueue path is weaker there, so those numbers aren't load-bearing.)

---

## Caveats
- Single runs, ±10–15% variance; `redis-server`/competitor versions are distro/Docker `:latest`.
- The ARM and x86 runs used different connection counts (40 vs 100) and arch, so they are not
  directly comparable to each other — each is internally consistent.
- For publication-grade numbers: bare-metal x86, pinned cores, multiple trials, and
  `memtier --hdr-file-prefix` for p50/p99/p99.9 (coordinated-omission aware).

## Roadmap to also beat Garnet
1. **Give the io_uring runtime the fast path** (borrowed `GET`, alloc-free `SET`/`INCR`) so it stops
   allocating a `Reply` per request — should lift it above the portable build.
2. **Single-owner shards** (ADR-002 D4): drop the per-op `Mutex` so each io_uring executor owns its
   shards lock-free — the real thread-per-core win, and the path past Garnet at `-P 64`.
3. **SwissTable-SIMD → dashtable index** for memory + probe speed.

## Reproduce
```bash
# x86 EC2 (or any Linux box): copy the repo + scripts/provision-linux.sh and run it,
# or replicate the run script used here (apt deps, build portable + --features io-uring,
# memtier across native + `docker run --network host` competitors).
bash scripts/provision-linux.sh
```
