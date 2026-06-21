//! `inmemd` — the thread-per-core RESP server (ADR-001).
//!
//! Thin wrapper: parse config, recover state, start the reaper, run the accept loop.

use inmem_server::config::{Config, HELP};
use inmem_server::server::Server;

/// A fast multi-threaded allocator. The cache is allocation-heavy (every SET stores boxed
/// key/value bytes); mimalloc cuts both latency and fragmentation versus the system allocator.
#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

fn main() {
    let cfg = match Config::from_args(std::env::args().skip(1)) {
        Ok(c) => c,
        Err(e) if e == "help" => {
            print!("{HELP}");
            return;
        }
        Err(e) => {
            eprintln!("error: {e}\n\n{HELP}");
            std::process::exit(2);
        }
    };

    let server = match Server::bootstrap(cfg) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("failed to start: {e}");
            std::process::exit(1);
        }
    };

    server.start_reaper();
    if let Err(e) = server.serve() {
        eprintln!("server error: {e}");
        std::process::exit(1);
    }
}
