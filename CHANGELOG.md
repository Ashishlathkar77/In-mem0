# Changelog

All notable changes to this project are documented here. Format based on
[Keep a Changelog](https://keepachangelog.com/); this project uses [SemVer](https://semver.org/)
(pre-1.0: minor = features, patch = fixes).

## [Unreleased]

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

### Known limitations
- Latency-bound (`-P 1`) throughput trails Redis; the io_uring thread-per-core runtime (Linux)
  is the planned fix.
- No auth/TLS/replication/clustering yet. String values only (no lists/hashes/sets yet).
