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
    server.start_replication();

    // On Linux with --features io-uring, use the thread-per-core io_uring runtime (ADR-002);
    // otherwise the portable thread-per-connection server. SYNC replication is served only by
    // the portable path, so a primary that needs replicas should run the portable build.
    #[cfg(all(target_os = "linux", feature = "io-uring"))]
    let result = inmem_server::runtime_uring::serve(server);
    #[cfg(not(all(target_os = "linux", feature = "io-uring")))]
    let result = server.serve();

    if let Err(e) = result {
        eprintln!("server error: {e}");
        std::process::exit(1);
    }
}
