# Research: what it takes to beat Garnet (and an honest plan)

Fact-checked deep research (101 agents, claims adversarially verified). Date: 2026-06-22.
Question: why is Garnet ~1.5× faster than our Rust RESP server at high pipelining, and what's
implementable to close/beat it?

## Verified finding: Garnet's edge is STRUCTURAL per-op CPU efficiency, not batching

Garnet outperforms even at batch=1 and the lead grows with sessions/batch — so it's not a
pipelining trick. Three mechanisms, ranked by expected impact:

### 1. Latch-free hash index (Tsavorite/FASTER) — highest impact
- 64-byte (cache-line) buckets: seven 8-byte entries + one 8-byte overflow pointer.
- Each entry packs 15-bit tag + tentative bit + 48-bit address (bits stolen from x86-64 pointers),
  so **every index mutation is a single 64-bit atomic CAS** — no lock word.
- Keys kept OUT of the index (in a separate log/arena) to keep it cache-dense.
- Memory reclamation via **epoch protection**: threads refresh a thread-local epoch (no shared
  counter write), avoiding the cache-line ping-pong that a shared latch's lock-word write causes.
- **This is exactly what limits our sharded `parking_lot::Mutex` design**: every GET/SET writes the
  shard's lock word, bouncing that cache line across cores under high pipelined load. CAS-on-the-slot
  + epoch reclamation removes it.
- Sources: FASTER SIGMOD'18; MSR FASTER blog; EPVS DaMoN'22; Garnet processing docs.

### 2. Zero per-command allocation + response coalescing — second, lower risk
- Pooled receive/send buffers; handlers write RESP responses directly into one sender-owned buffer;
  many pipelined replies flushed in a single send.
- **Status in inmem:** largely already done — the conn loop reuses in/out buffers, parses many
  commands per read, coalesces all replies into one `outbuf`, single write; the fast path
  (`serve_fast`) avoids `Reply` allocation for GET/SET/INCR. Remaining: a per-command `Vec<&[u8]>`
  argv allocation (addressed with `SmallVec`, below) and `writev`/scatter-gather.

### 3. Run-to-completion threading (shared memory, IO thread does parse+lookup+serialize) — third, biggest lift
- Garnet runs network IO + parse + storage on the same IO-completion thread; cache coherence moves
  data to the network rather than shuffling to a shard owner.
- **Status in inmem:** the portable server is already run-to-completion *per connection* (one thread
  does parse→lookup→serialize). The full win needs a bounded thread-per-core IO model (the io_uring
  runtime is the start) AND the latch-free index so cores don't serialize on locks.

## Honest assessment

- **The decisive lever is #1, the latch-free index.** It's also the hardest and most
  correctness-critical change — hand-rolling a lock-free open-addressing index with epoch-based
  memory reclamation (or integrating one), while preserving TTL, multi-type values, and S3-FIFO
  eviction, is a multi-day-to-multi-week effort that MUST be validated with concurrency testing
  (e.g. `loom`) and stress tests. Rushing it risks data races — worse than being an honest #2.
- **#2 micro-optimizations** (SmallVec argv, writev) are safe and cheap but will only shave a few
  percent — they will not close a 1.5× gap alone.
- Even with the latch-free index, beating Garnet is not guaranteed: Garnet also has a very mature,
  heavily tuned network layer. Expect to *narrow* the gap substantially; *beating* it is plausible
  but must be proven on the clean two-machine rig, not assumed.

## Caveats (from the research)
- Garnet's headline numbers are Microsoft's own benchmarks (uniform-random, Azure F72s v2);
  directionally corroborated by an independent 2025 study (arXiv 2510.19805).
- "Latch-free" is precise for the index probing/mutation hot path; FASTER still uses transient
  record locks for some large-value/non-atomic cases.
- Don't hardcode 48 pointer bits (differs on 5-level paging / non-x86).
- A latch-free *shared* index may concentrate CAS contention on a single hot key's cache line under
  skew, where a sharded design distributes it — measure both on skewed workloads.

## Recommended plan
1. (Safe, now) `SmallVec` argv to remove the per-command heap allocation on the hot path.
2. (The real lever, scoped as a serious effort) A latch-free index: an arena/log for records +
   an open-addressing index of `AtomicU64` slots (tag + arena offset) mutated by `compare_exchange`,
   with `crossbeam-epoch` reclamation; re-integrate TTL/eviction. Gate behind a feature, test with
   `loom` + stress, then A/B on the two-machine rig.
3. `writev` scatter-gather + io_uring registered buffers/multishot recv as follow-ons.

Sources: FASTER SIGMOD'18 (microsoft.com/research), MSR FASTER blog, EPVS DaMoN'22 (badrish.net),
Tsavorite/Garnet docs (microsoft.github.io/garnet), arXiv 2305.01516, arXiv 2510.19805.
