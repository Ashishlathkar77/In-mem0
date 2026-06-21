# In-Memory Cache Framework — Research Foundation (ASH-309)

> Deep, fact-checked research synthesizing how the fastest in-memory KV stores beat Redis,
> and which academic/industry techniques are worth adopting. 25 claims adversarially
> verified (3-vote, need 2/3 to kill): 23 confirmed, 2 refuted. Date: 2026-06-21.

## TL;DR

The fastest in-memory KV stores beat Redis along **three axes**:

1. **Concurrency architecture** — multithreaded shared-nothing (thread-per-core) or
   epoch-based latch-free cores scale throughput near-linearly with cores, where Redis's
   single-threaded event loop plateaus.
2. **Hash-table & memory layout** — flat/open-addressed segmented tables avoid Redis's
   per-entry chaining and double-and-rehash latency spikes; cut per-item overhead and
   support incremental, spike-free resize.
3. **Eviction policy** — modern FIFO/sketch-based eviction (S3-FIFO, SIEVE, W-TinyLFU)
   beats LRU/ARC/LIRS on *both* miss ratio and multi-core throughput by removing per-hit
   pointer promotion and its locking.

## How the fastest systems win

| System | Key idea | Evidence |
|---|---|---|
| **Garnet** (MS) | RESP-compatible, built on FASTER/Tsavorite; better throughput scalability than Redis 7.2 & KeyDB 6.3.4 from 1→128 sessions | Vendor bench + arXiv 2510.19805 (+108–110% over Redis vs KeyDB ~12–15%). Throughput only — its *low-latency* claim was REFUTED 0-3. Numbers use heavy batching (4096 req/batch). |
| **FASTER** (MS) | Epoch-based latch-free fast path (no sync most of the time); up to 160M ops/sec single machine | SIGMOD'18. Caveat: embedded-API microbench, not networked vs Redis. |
| **Dragonfly** | Thread-per-core shared-nothing + extendible-hashing **dashtable**; eviction co-designed into the table (2Q) | First-party docs, mechanistic + independently corroborated. |
| **Memcached** | Slab allocator + multithreaded LRU | baseline multithreaded comparison |
| **KeyDB** | Multithreaded fork of Redis | scales only ~12–15% vs Redis in the arXiv study |

## Highest-leverage techniques to adopt (verified)

### Concurrent hash index
- **Dragonfly dashtable** (extendible hashing, 1979): dynamic array of constant-size flat
  hash-tables. Per-item overhead **6–16 bytes** (vs Redis dict 16–32); directory overhead
  collapses (~9.6KB vs ~8MB for 1M items); **incremental constant-size segment allocation
  eliminates rehash latency spikes**. Segment = 56 regular + 4 stash buckets × 14 slots = 840 records.
- **Optimistic cuckoo hashing** (MemC3 NSDI'13, libcuckoo EuroSys'14): ~95% table
  occupancy; lock-free multi-reader reads; libcuckoo adds concurrent writers, beats Intel
  TBB by 2.5× using <½ the memory for 64-bit pairs; ~40M inserts/s & >70M lookups/s on 16
  cores. MemC3 full system: 30% less memory, up to 3× QPS vs stock Memcached. BFS cuckoo
  pathfinding cuts under-lock displacements from 250 → ≤5 (shorter critical section = lower tail).
- **FASTER index** (SIGMOD'18): 64-byte cache-line buckets = seven 8-byte entries +
  overflow pointer; pointer-bit-stealing (48-bit addrs) so entries fit 64 bits and update
  **latch-free via a single 64-bit CAS**. **HybridLog**: in-place updates for hot records +
  append-only log for cold; supports larger-than-memory data and a drifting hot set.

### Eviction / admission
- **S3-FIFO** (SOSP'23): three static FIFO queues (small/main/ghost). Most efficient on
  10/14 datasets (6594 traces) at 10% cache; Cachelib prototype **>6× LRU throughput on 16
  cores** because FIFO enables lock-free implementations.
- **SIEVE** (NSDI'24): FIFO + a non-restarting "hand"; no locking on hits; **2× optimized-LRU
  throughput on 16 threads**; lower miss ratio than 9 SOTA on >45% of 1559 traces (next-best
  TwoQ 15%). NOTE: the "63.2% vs LRU" figure was REFUTED — do not cite it.
- **W-TinyLFU** (Caffeine): Count-Min Sketch approx-LFU admission; metadata for millions of
  items fits one pinned page; ~3-bit capped counters + Doorkeeper cut metadata ~89%. Tops/equals
  LRU/ARC/LIRS — but the "consistently best" claim was 2-1: the fixed 1% window underperforms on
  OLTP/F1/F2 and needs per-workload window tuning (20–40%).
- **Dragonfly 2Q-in-dashtable**: eviction built INTO the hash table (slot ranking + segment
  stash as probationary buffer, ~6.7% of space) → **zero per-item memory overhead, O(1) runtime,
  no auxiliary LRU lists/pointers**. Key lesson: co-design eviction with the index.

## Synthesized architecture recommendation (medium confidence — composed of high-confidence parts)

Build the Redis-beater around:
- **(a)** thread-per-core/shared-nothing **OR** epoch-based latch-free core (throughput scaling)
- **(b)** segmented flat/open-addressed hash index (dashtable-style extendible hashing, or
  cuckoo / FASTER 64-bit-CAS entries) for low memory overhead + incremental spike-free resize
- **(c)** eviction co-designed into the index: lock-free FIFO (S3-FIFO or SIEVE), optionally
  fronted by W-TinyLFU admission
- **(d)** RESP2/RESP3 wire compatibility + fork-COW snapshot / AOF-style durability

## Honest gaps / caveats (what this research did NOT establish)

- **Language choice (Rust vs C++ vs Zig): NO verified evidence.** Recommendation is reasoned
  inference, not data. This is the least-evidenced part of the report.
- Nothing verified on: allocators (jemalloc/mimalloc/tcmalloc), networking/kernel-bypass
  (io_uring/DPDK/zero-copy), RESP2/RESP3 protocol specifics, persistence internals
  (AOF/RDB/COW fork mechanics), NUMA, Aerospike/Hazelcast, Memcached slab details,
  benchmarking methodology / coordinated omission.
- Dragonfly evidence is first-party (mitigated: mechanistic + corroborated). Garnet numbers
  are vendor, throughput-only, heavily batched. FASTER 160M is embedded microbench. Several
  headline numbers (cuckoo 40M/70M, FASTER 160M) date 2014–2018 — architectural evidence,
  not current records.

## Open questions to resolve before/while building

1. Rust vs C++ vs Zig — direct benchmark/engineering evidence on throughput, tail latency,
   memory-safety tradeoffs for a thread-per-core KV store.
2. Thread-per-core (Dragonfly/Seastar) vs epoch-based latch-free (FASTER/Garnet) on
   **tail latency (p99/p99.9)** under coordinated-omission-correct benchmarking.
3. Allocator strategy (jemalloc/mimalloc/tcmalloc/custom slab/log-structured à la MICA/RAMCloud)
   for a flat-segmented design; is compaction needed?
4. Networking stack (io_uring vs epoll vs DPDK) + RESP pipelining/batching; durability
   (fork-COW vs WAL/AOF vs replication) without reintroducing fork/resize latency spikes.

## Primary sources
- Dragonfly dashtable: https://github.com/dragonflydb/dragonfly/blob/main/docs/dashtable.md
- Dragonfly cache design: https://www.dragonflydb.io/blog/dragonfly-cache-design
- Garnet bench: https://microsoft.github.io/garnet/docs/benchmarking/results-resp-bench
- Garnet study: https://arxiv.org/abs/2510.19805
- FASTER SIGMOD'18: https://www.microsoft.com/en-us/research/uploads/prod/2018/03/faster-sigmod18.pdf
- MemC3 NSDI'13: https://www.cs.cmu.edu/~dga/papers/memc3-nsdi2013.pdf
- libcuckoo EuroSys'14: https://www.cs.princeton.edu/~mfreed/docs/cuckoo-eurosys14.pdf
- S3-FIFO SOSP'23: https://www.cs.cmu.edu/~rvinayak/papers/s3-fifo-sosp-2023-fifo-queues-are-all-you-need-for-cache-eviction.pdf
- SIEVE NSDI'24: https://www.usenix.org/system/files/nsdi24-zhang-yazhuo.pdf
- TinyLFU: https://arxiv.org/pdf/1512.00727
- Caffeine efficiency: https://github.com/ben-manes/caffeine/wiki/Efficiency
- MICA NSDI'14: https://www.usenix.org/system/files/conference/nsdi14/nsdi14-paper-lim.pdf
- Seastar shared-nothing: https://seastar.io/shared-nothing/
- Coordinated omission: https://www.scylladb.com/2021/04/22/on-coordinated-omission/
