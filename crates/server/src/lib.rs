//! `inmem-server` — library surface for the `inmemd` binary and integration tests.
//!
//! The thread-per-core RESP server (ADR-001 D3/D4/D7) that wraps `inmem-core`. The binary in
//! `main.rs` is a thin wrapper around [`server::Server`].

pub mod commands;
pub mod config;
pub mod conn;
pub mod persistence;
pub mod server;

pub use config::Config;
pub use server::Server;
