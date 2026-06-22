# Benchmarks — honest status

Reproduce with `scripts/bench-all.sh` (macOS/native) or `scripts/provision-linux.sh` (Linux VM).
Numbers are representative single runs and vary ±10–15%. **Platform matters a lot** — results below
are reported per-platform, and the Linux numbers are the ones to trust for production intent.

Driver: `memtier_benchmark`, mixed **GET/SET 1:1**, 64-byte values, ~40 connections.

---

## Linux (the result that matters) — 8-core ARM VM, kernel 6.8

Native = host networking (inmem, redis, memcached). Docker = run via container (Valkey, KeyDB,
Dragonfly, Garnet) — **these pay a Docker NAT/userland-proxy penalty, especially at `-P 1`**, so they
are *not* directly comparable to the native rows; treat them as a rough floor, not a ceiling.

| system | net | `-P 1` | `-P 16` | `-P 64` |
|---|---|--:|--:|--:|
| **redis 7.x**       | native | **467k** | **2.93M** | **4.11M** |
| **inmem (portable)**| native | 291k | 2.65M | 3.84M |
| memcached           | native | 376k | 1.55M | 1.73M |
| inmem (io_uring)    | native | 138k | 1.73M | 3.33M |
| garnet              | docker | 128k | 1.57M | 4.01M |
| valkey              | docker | 145k | 1.55M | 3.21M |
| keydb               | docker | 137k | 1.42M | 2.52M |
| dragonfly           | docker | 111k | 0.94M | 2.26M |

### Honest conclusions (Linux)
- **inmem does NOT currently beat Redis on Linux.** redis is fastest in this test; inmem-portable is
  a close second — within ~7% at `-P 64` and ~10% at `-P 16`, but ~38% behind at `-P 1` (the
  latency-bound, thread-per-connection weak spot).
- **The io_uring runtime (v1) is slower than the portable build, not faster.** Root cause: the
  glommio handler routes every command through the allocating `dispatch` path and still shares the
  `Mutex`-protected store across executors — it has neither the portable build's zero-copy fast path
  (borrowed `GET`, alloc-free `SET`/`INCR`) nor single-owner shards. So it adds executor overhead
  without the lock-free win. Fixing this is the next step (see Roadmap).
- **Garnet is excellent** at high pipelining (4.01M at `-P 64` even through Docker). Redis and Garnet
  set the bar to beat.

---

## macOS — Apple Silicon (does NOT generalize to Linux)

On macOS, inmem *led* at `-P 16`/`-P 64` (e.g. inmem 3.39–4.05M vs redis ~2.4–2.8M at `-P 64`).
That advantage is **macOS-specific** — Redis's event loop is less optimized on macOS (kqueue) than on
Linux (epoll), and our multi-threaded model benefited. **It did not hold up on Linux**, which is why
on-platform benchmarking matters and why no production claim should rest on macOS numbers.

---

## What this means / roadmap to actually compete with Redis & Garnet

The portable build is already a close second to Redis on Linux. To pull even and ahead:

1. **Give the io_uring runtime the fast path.** Port `conn::serve_fast` (borrowed `GET` via
   `Store::read_str`, alloc-free `SET`/`INCR`) into `runtime_uring::handle_conn` so it stops
   allocating a `Reply` per request. Expected: io_uring catches and passes the portable build.
2. **Single-owner shards (ADR-002 D4).** Split the store so each glommio executor owns its shards
   with no `Mutex` — removes the per-op lock that caps multi-core scaling. This is the real
   thread-per-core win and the path to beating Redis at `-P 1`.
3. **SwissTable-SIMD → dashtable index** for memory + probe-speed.
4. Re-benchmark on **bare-metal x86 Linux** (not an ARM VM) with `--hdr-file-prefix` for honest
   p50/p99/p99.9, and run all competitors on host networking (not Docker) for a fair cross-compare.

## Reproduce
```bash
# Linux (free VM): see scripts/provision-linux.sh and the runbook in the README
bash scripts/provision-linux.sh
# macOS:
scripts/bench-all.sh 1000000 50
```
