# Contributing to inmem

Thanks for your interest in making the fastest, most reliable open-source cache. This project is
built to be **welcoming, well-tested, and honest about performance**.

## Ground rules

- **Correctness first, then speed.** Every behavior change needs a test. Performance claims need
  a reproducible benchmark (`scripts/bench-all.sh`), not vibes.
- **No unsafe without justification.** This is a Rust project chosen partly for memory safety
  (see [ADR-001](docs/architecture/ADR-001-foundations.md)). If you add `unsafe`, document the
  invariant it upholds and why safe code can't do it, and cover it with tests.
- **Keep the hot path allocation-free.** GET/SET/INCR must not allocate per request. If a change
  adds allocation to `crates/server/src/conn.rs` or `crates/core/src/store.rs`, call it out.

## Developer setup

```bash
rustup toolchain install stable      # pinned in rust-toolchain.toml (1.96+)
cargo build
cargo test                           # unit + differential fuzz + TCP e2e
cargo fmt --all                      # required before pushing
cargo clippy --all-targets           # must be warning-clean (CI enforces)
```

## Before you open a PR

1. `cargo fmt --all && cargo clippy --all-targets -- -D warnings`
2. `cargo test` — all green.
3. If you touched the hot path, run `scripts/bench-all.sh` and paste before/after numbers.
4. Write a descriptive PR: **what** changed, **why**, **how**, and **testing done**.

## Commit messages

`type(scope): summary` where type ∈ {feat, fix, perf, refactor, docs, test, chore}. Body explains
what/why/how. Example: `perf(core): SIMD control-byte probing in FlatMap`.

## Project layout

| Path | What |
|---|---|
| `crates/core` | embeddable cache: hash index, S3-FIFO eviction, sharded store |
| `crates/proto` | RESP2/3 parse + encode |
| `crates/server` | `inmemd` server, command dispatch, persistence |
| `docs/` | architecture decision records, research, benchmarks |
| `scripts/` | benchmark harnesses |

## Good first issues

- Implement a new command (see the dispatch table in `crates/server/src/commands.rs`).
- Add a data type (lists/hashes/sets) behind the shard API.
- Improve the glob matcher, add `OBJECT ENCODING`, etc.
- Port the io_uring thread-per-core runtime (Linux) — the big perf item.

By contributing you agree your work is licensed under the project's [Apache-2.0](LICENSE) license.
