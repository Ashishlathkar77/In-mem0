# How to beat Garnet — the concrete recipe (research #2)

Fact-checked deep research (105 agents). Date: 2026-06-22. Builds on research/02.

## The #1 rule the research insists on: PROFILE FIRST

> "Where is the actual bottleneck in the current ~8–10M server — index lock contention (which the
> latch-free swap fixes), or syscall/NIC/RESP-encode overhead (which it does not)? This must be
> profiled before committing engineering effort, because the entire recipe assumes the index is the
> binding constraint."

So before any multi-day lock-free rewrite, run `perf` on the server under load and look at the top
symbols. If `parking_lot`/futex/lock shows up hot → the index swap is the win. If it's
`read`/`write` syscalls, `memcpy`, ahash, or RESP encode → the index won't help and we attack that
instead.

## Ranked recipe (once profiling confirms the index is the constraint)

1. **Swap the sharded `parking_lot` store for a lock-free-read index.** Two options:
   - **`papaya` crate (recommended first try, low risk):** lock-free reads that "never block under
     any circumstances," open-addressing + SwissTable-style metadata, best-in-class on read-heavy
     (98/1/1) cache benchmarks, beats DashMap. No `unsafe` epoch code to hand-write. Risk: weaker on
     update-heavy workloads (reclamation pressure) — eviction/TTL churn can erode the edge.
   - **Hand-built FASTER/Tsavorite index (higher effort, the proven blueprint):** cache-aligned
     64-byte buckets, seven `AtomicU64` entries (15-bit tag + 48-bit arena offset), every mutation a
     single 64-bit CAS, records in an arena, `crossbeam-epoch` reclamation. "Almost-latch-free"
     (EPVS/F2): latch-free common path, lock only for rare resize. Needs `loom` + stress testing.
   - **`leapfrog` LeapMap** is fastest measured (148 Mops/s, 16-core) but only 64-bit Copy values →
     usable only as the integer index layer pointing into an arena, not the value store.
2. **Thread-per-core io_uring** (Monoio/Glommio): near-linear scaling vs Tokio (~2× at 4 cores, ~3×
   at 16 cores on small messages). We already have a glommio runtime; pair it with single-owner
   shards so cores don't share the index.
3. **io_uring `multishot recv`** for sub-1KiB RESP: ~6–8% QPS. **Avoid `SEND_ZC`/registered send
   buffers for tiny values** — below ~1KiB they're *slower* (page-pinning overhead > copy savings).

## Validate the map choice with data, not faith
Run candidates (papaya, scc, leapfrog) through **`conc-map-bench`** (read-heavy 98/1/1, the cache
profile) at our target core count before committing. The headline map numbers are author
microbenchmarks (map-internal, not end-to-end RESP) — they will NOT translate 1:1.

## Honest probability: MEDIUM (plausible, not guaranteed)
- The index swap is the best throughput-per-effort lever IF the index is the bottleneck.
- Map microbenchmark wins shrink once hashing + RESP parse/encode + syscalls + NIC are added.
- Garnet's network layer is very mature; even a perfect index may not fully close 1.5×.
- Refuted/》not-targets: FASTER "160M ops/s" and F2 "2–11.9×" did not survive verification — treat as
  architectural proof points, not ceilings.

## Where Garnet may be beatable first (under-evidenced — verify)
The research did NOT surface concrete "Garnet loses" benchmarks, so this is hypothesis: single
data-structure ops at very high core counts, memory efficiency, and tail latency are likelier first
wins than raw uniform GET/SET throughput where Garnet is strongest. Confirm by measurement.

## Decision
Next action = **profile under load** (perf flamegraph) to confirm the bottleneck. Then, if
lock-bound, **try `papaya` first** (low risk) and A/B on the two-machine rig before committing to the
hand-built FASTER index.

Sources: papaya design doc + BENCHMARKS.md; leapfrog/conc-map-bench (robclu); FASTER SIGMOD'18;
EPVS DaMoN'22; F2 (FASTER v2); Monoio/Glommio benchmarks; liburing multishot/SEND_ZC notes.
