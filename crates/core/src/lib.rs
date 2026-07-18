//! `inmem-core` — the embeddable in-memory cache core.
//!
//! The reusable library at the center of the project (ADR-001 D2): a fast, dependency-light
//! cache that the `inmem-server` RESP shell wraps. Components:
//!
//! - [`map::FlatMap`] — flat open-addressing hash index (tombstone-free backward-shift deletion).
//! - [`s3fifo::S3Fifo`] — lock-free-friendly FIFO eviction policy (SOSP'23).
//! - [`store::Store`] — sharded, TTL-aware, memory-bounded key/value store tying them together.
//!
//! The store shards are individually locked today; the thread-per-core endgame (ADR-001 D4)
//! replaces those locks with single-owner access without changing the public API.

pub mod map;
pub mod s3fifo;
pub mod store;

pub use map::FlatMap;
pub use s3fifo::S3Fifo;
pub use store::{fmt_score, now_ms, SetOptions, Store, StoreStats, StrRead, Ttl, Value, WRONGTYPE};
