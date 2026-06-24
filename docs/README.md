# inmem documentation

Start at the [project README](../README.md) for install & usage. This folder holds the deeper
design and measurement docs.

## Architecture (decision records)
- [ADR-001 — Foundations](architecture/ADR-001-foundations.md) — language, form factor, sharding,
  index, eviction, protocol, persistence choices.
- [ADR-002 — io_uring thread-per-core runtime](architecture/ADR-002-io-uring-thread-per-core.md) —
  the Linux high-performance networking path (`--features io-uring`).

## Benchmarks
- [BENCHMARKS.md](BENCHMARKS.md) — methodology, the two-machine results vs Redis/Valkey/KeyDB/
  Memcached/Dragonfly/Garnet, and honest caveats.

## Research (fact-checked surveys behind the design)
- [01 — Cache landscape & techniques](research/01-landscape-and-techniques.md)
- [02 — What it takes to beat Garnet](research/02-beating-garnet.md)
- [03 — The how-to-win recipe](research/03-winning-recipe.md)
- [04 — Garnet network breakdown](research/04-garnet-network-breakdown.md)

## Contributing & policies
- [CONTRIBUTING](../CONTRIBUTING.md) · [CODE_OF_CONDUCT](../CODE_OF_CONDUCT.md) ·
  [SECURITY](../SECURITY.md) · [CHANGELOG](../CHANGELOG.md)
