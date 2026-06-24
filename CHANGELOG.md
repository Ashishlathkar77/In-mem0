# Changelog

All notable changes to this project are documented here. Format based on
[Keep a Changelog](https://keepachangelog.com/); this project uses [SemVer](https://semver.org/)
(pre-1.0: minor = features, patch = fixes).

## [0.1.1] — 2026-06-24
### Added
- Native **Windows** (`x86_64`) prebuilt binary; CI now builds/tests on Windows too.
  inmem now ships native binaries for Linux (x86_64/aarch64), macOS (x86_64/aarch64), and Windows.

## [0.1.0] — 2026-06-24

First public release. Standalone Redis-compatible (RESP2/3) cache server + embeddable core.
Distributed as a multi-arch Docker image (GHCR) and prebuilt binaries.

### Added
- **Embeddable core** (`inmem-core`): flat open-addressing hash index with tombstone-free
  backward-shift deletion; S3-FIFO eviction (SOSP'23); sharded TTL- and memory-bounded store.
- **Protocol** (`inmem-proto`): RESP2/RESP3 parser (array + inline) and encoder; zero-copy
  range parser and allocation-free reply writers.
- **Server** (`inmemd`): multithreaded RESP server with the common string/keyspace command set
  (`SET GET INCR/DECR APPEND STRLEN MSET MGET DEL EXISTS TYPE EXPIRE/TTL KEYS SCAN …`),
  `HELLO/CONFIG/CLIENT/COMMAND/INFO` for client & tooling compatibility, `maxmemory` eviction,
  TTL with a background reaper, AOF persistence, and binary snapshots with startup recovery.
- **Performance**: mimalloc allocator, zero-copy request parsing, borrowed `GET` path. Beats
  Redis at high pipelining (`-P 64`: SET +23%, GET +16% on the dev box).
- **Benchmarks**: `scripts/bench.sh` (vs Redis) and `scripts/bench-all.sh` (vs Redis, Valkey,
  KeyDB, Memcached via memtier; Dragonfly/Garnet documented as Docker-based).
- Docs: research survey, ADR-001 architecture, benchmarks, contributing/security/CoC.

- **Data types**: lists (LPUSH/RPUSH/LPOP/RPOP/LLEN/LRANGE), hashes (HSET/HGET/HMGET/HDEL/
  HGETALL/HKEYS/HVALS/HLEN/HEXISTS), sets (SADD/SREM/SISMEMBER/SCARD/SMEMBERS), sorted sets
  (ZADD/ZSCORE/ZREM/ZCARD/ZRANGE [WITHSCORES]). `WRONGTYPE` errors; `TYPE`.
- **AUTH** via `--requirepass` (NOAUTH gate).
- **Async replication**: `--replicaof host:port` read-only replicas with initial snapshot sync +
  live command streaming; `--masterauth`; `-READONLY` on replica writes.
- **TLS** (optional, `--features tls`): rustls-based encrypted client connections.
- Type-generic snapshots (RESP reconstruction-command dumps).

### Known limitations
- Latency-bound (`-P 1`) throughput trails Redis/Valkey; the io_uring thread-per-core runtime
  (ADR-002, Linux) is the planned fix and is designed/scaffolded but not yet built.
- Replication has no backlog/offset (a write racing the initial handshake may double-apply);
  steady-state is exactly-once. No clustering/pub-sub yet.
