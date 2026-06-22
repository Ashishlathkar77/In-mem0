# ADR-002 — io_uring thread-per-core runtime (Linux)

Status: **Implemented (Linux), pending on-host validation** · Date: 2026-06-21 · Supersedes the networking half of ADR-001 D3/D4

> Implemented in [`crates/server/src/runtime_uring.rs`](../../crates/server/src/runtime_uring.rs)
> behind `--features io-uring` (Linux-only). It could not be compiled or benchmarked on the macOS
> dev box (no io_uring), so a GitHub Actions Linux job compile-checks it and `scripts/provision-linux.sh`
> builds + runs it on a free Linux VM. Performance numbers will be filled in once measured there.

## Why

Benchmarks ([docs/BENCHMARKS.md](../BENCHMARKS.md)) show inmem already beats Redis/Valkey/KeyDB/
Memcached at `-P 16` and `-P 64`, but **trails at `-P 1`** (one request per round-trip). Root cause:
the v1 server is **thread-per-connection with blocking sockets**, so each request pays a
`read`/`write` syscall and the kernel context-switches among many threads. Redis's single epoll
loop avoids that. The fix is a **thread-per-core, shared-nothing runtime on io_uring**:

- One worker thread pinned per core, each with its own io_uring submission/completion queues.
- Each worker owns a fixed subset of **shards**; a key routes to its owning core, so the hot path
  is **lock-free** (no shard `Mutex`).
- Batched submit/complete amortizes syscalls across many in-flight requests, collapsing the
  per-op overhead that hurts `-P 1` and lifting the multi-core ceiling further.

This is the single highest-leverage remaining performance item.

## Honesty note

This ADR is **design + scaffold only**. It was authored on macOS, which has no io_uring, so the
runtime cannot be compiled or benchmarked from the current environment. It is gated behind
`#[cfg(all(target_os = "linux", feature = "io-uring"))]` so it never affects the default build,
and a Linux CI job (see `.github/workflows/ci.yml`) compile-checks it. Do not claim io_uring
performance numbers until they are measured on a Linux host.

## Design

### Crate choice
Two viable options, both Linux-only:
- **`glommio`** — a batteries-included thread-per-core async runtime on io_uring (executors,
  pinning, async TCP). Highest-level, least error-prone. **Recommended for v1.**
- **`io-uring`** (raw) or **`monoio`** — more control, more code. Defer unless glommio limits us.

### Architecture
```
                 ┌─ core 0: glommio executor ── io_uring ── conns ── shards {0,4,8,..}
 SO_REUSEPORT ───┼─ core 1: glommio executor ── io_uring ── conns ── shards {1,5,9,..}
   (per core)    ├─ core 2: ...
                 └─ core N: ...
```
- **Accept**: each worker opens the listen port with `SO_REUSEPORT`; the kernel load-balances new
  connections across workers. No shared accept lock.
- **Shard ownership**: `Store` is split so each worker owns `shards where shard_id % num_workers
  == worker_id`. A request for a key on another worker's shard is forwarded via a per-worker SPSC
  ring (rare for well-distributed keys); alternatively, start with **per-worker independent
  stores** keyed by `SO_REUSEPORT` placement and accept cross-core forwarding as a later step.
- **Lock removal**: because a shard has a single owner thread, the `Mutex<Shard>` becomes plain
  `RefCell`/`UnsafeCell` ownership — the documented end state of ADR-001 D4.

### Integration points (reuse, do not rewrite)
- **Command dispatch** ([commands.rs](../../crates/server/src/commands.rs)) is already
  transport-agnostic (`dispatch(store, &[&[u8]], st, cfg) -> Outcome`) — reuse verbatim.
- **Zero-copy parser** (`parse_command_ranges`) and **direct writers** (`write_bulk` etc.) are
  already allocation-free — reuse verbatim.
- Only the **I/O loop** ([conn.rs](../../crates/server/src/conn.rs)) is replaced: read into a
  per-connection buffer, parse-dispatch-encode into an out buffer, submit a single write.

### Module layout (scaffold)
```
crates/server/src/runtime_uring.rs   #[cfg(all(target_os="linux", feature="io-uring"))]
  - fn serve(server: Arc<Server>) -> io::Result<()>
      spawn one pinned executor per core; each runs accept_loop()
  - async fn accept_loop(...)   // SO_REUSEPORT listener -> spawn handle_conn per accept
  - async fn handle_conn(...)   // mirror conn::run but with glommio async TCP
```
`main.rs` selects the runtime: io_uring on Linux when the feature is on, else the portable
thread-per-connection server.

## Validation plan (on a Linux host)
1. `cargo build --features io-uring` (CI compile-check).
2. `scripts/bench-all.sh` on a multi-core Linux box; compare `-P 1/16/64` vs the portable build
   and vs Redis/Dragonfly/Garnet (Docker — [scripts/bench-docker.md](../../scripts/bench-docker.md)).
3. Tail latency with `memtier_benchmark --hdr-file-prefix` (p50/p99/p99.9), coordinated-omission
   aware. Target: match or beat Redis at `-P 1` and widen the lead at high pipelining.

## Rollout
Keep the portable server as the default and cross-platform path. io_uring is an opt-in Linux build
(`--features io-uring`) until it is measured and proven, then it becomes the default on Linux.
