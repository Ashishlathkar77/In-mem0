# Benchmarks — honest status

Driver: `memtier_benchmark`, mixed **GET/SET 1:1**, 64-byte values. The authoritative numbers are
from a **two-machine setup** (load generator on a *separate* box) — colocating the client with the
servers caps throughput and hides real differences (it made inmem look tied with Garnet when it
isn't). Single-machine numbers below are kept only as a cautionary record.

---

## AUTHORITATIVE: two-machine, AWS c7i.4xlarge ×2 (16 vCPU x86 each)

Dedicated 16-vCPU client → server over the private network. memtier `-t 16 -c 16` (256 conns),
100k req/conn. Docker competitors on `--network host`. ops/sec, higher is better.

| system | `-P 1` | `-P 16` | `-P 64` |
|---|--:|--:|--:|
| **garnet**          | 1.02M | **10.3M** | **15.3M** |
| inmem (portable)    | 1.04M | 7.76M | 9.78M |
| inmem (io_uring)    | 1.02M | 8.26M | 9.66M |
| redis 7.x           | 263k  | 1.66M | 2.33M |
| memcached           | 1.03M | 1.33M | 1.32M |

### Honest verdict
- **inmem does NOT beat Garnet.** Measured cleanly, Garnet is ~1.3× faster at `-P 16` and ~1.5×
  faster at `-P 64`. Garnet's highly-tuned thread-per-core network/storage layer is genuinely ahead;
  beating it is an open, hard problem — not achieved here.
- **inmem is a strong #2.** It beats Redis ~4× at `-P 64`, Memcached ~7× at `-P 16`/`-P 64`, and
  (earlier runs) Valkey/KeyDB/Dragonfly. At `-P 1` everything clusters ~1M (cross-machine round-trip
  bound).
- **The `parking_lot` spin-lock fixed the io_uring runtime**: it now matches the portable build
  (8.26M vs 7.76M at `-P 16`), where before it trailed (std `Mutex` parking stalled executor cores).
  Neither config beats Garnet, though.
- **Lesson:** the earlier "tie with Garnet" and the macOS "beats everyone" were measurement
  artifacts (colocated client / weak macOS event loop). Trust the two-machine rig.

### What it would take to beat Garnet (honest, hard)
Not a quick tweak. Garnet leads by ~1.5×, which points to deeper work: single-owner lock-free
shards (no per-op atomic at all), a custom batched network layer (Garnet's is heavily optimized),
SIMD/cache-optimal index (SwissTable→dashtable), and careful pipeline batching. Multi-week effort
with uncertain payoff against a mature research system.

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
  level that involves real batching — and beats Garnet at `-P 1` and `-P 16` too. These gaps are
  2–3× and hold across runs.
- **At `-P 64`, inmem and Garnet are co-fastest (a statistical tie), not a Garnet win.** The
  `4.68M vs 5.08M` row above is **client-bound**: memtier with 4 threads can't push past ~5M, so it
  caps both servers. Removing that bottleneck (more memtier threads) the *servers'* real ceilings
  appear — and they're neck-and-neck:

  | memtier threads (P64) | inmem-portable | garnet |
  |---|--:|--:|
  | t=4 (client-bound) | 4.8M | 5.0M |
  | t=8  | 7.3M | 7.4M |
  | t=12 | 7.6M | 8.6M |
  | t=16 | 6.7–9.9M | 7.5–9.0M |

  At t=16 the run-to-run variance (±20%) **exceeds** the inmem↔garnet difference, because the client,
  server, and Garnet container all share the same 16 cores. **Honest verdict: inmem and Garnet are
  the two fastest and indistinguishable at extreme pipelining on this rig; inmem is clearly ahead of
  everything else.** A clean separation needs a proper setup (load generator on a *separate* machine,
  pinned cores, many trials) — noted as future work, not claimed here.
- **Memcached** has the best single-op `-P 1` (445k) but doesn't scale with pipelining.
- **The io_uring runtime is ~10–20% slower than the portable build** — it routes through the
  allocating `dispatch` path and shares the `Mutex` store (no lock-free shards). The portable build
  is the fast config today; making io_uring the fastest needs single-owner shards (ADR-002 D4).

> Note: these numbers used the new **512-shard default** (raising shard count cut Mutex contention
> substantially — an 8-core VM saw portable P64 rise ~3.7M→5.6M from the change).

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
