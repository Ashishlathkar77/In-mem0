//! Server assembly: shared state, startup recovery, accept loop, background reaper.

use crate::config::Config;
use crate::persistence::Aof;
use crate::repl::Replication;
use inmem_core::Store;
use std::net::TcpListener;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

/// Shared, immutable-after-startup server state handed to every connection thread.
pub struct Server {
    pub store: Store,
    pub config: Config,
    pub aof: Option<Aof>,
    /// Registry of connected replicas (this node acting as a primary).
    pub repl: Replication,
    /// True when running as a replica — rejects writes from normal clients.
    pub read_only: bool,
}

impl Server {
    /// Build the store and recover state from disk (AOF preferred, else snapshot).
    pub fn bootstrap(config: Config) -> std::io::Result<Arc<Server>> {
        let store = Store::new(config.shards, config.maxmemory);

        if config.appendonly && config.aof_path().exists() {
            let n = crate::persistence::load_aof(&config.aof_path(), &store, &config)?;
            eprintln!(
                "recovered {n} commands from AOF {}",
                config.aof_path().display()
            );
        } else if config.snapshot_path().exists() {
            let n = crate::persistence::load_snapshot(&store, &config.snapshot_path(), &config)?;
            if n > 0 {
                eprintln!(
                    "loaded {n} keys from snapshot {}",
                    config.snapshot_path().display()
                );
            }
        }

        let aof = if config.appendonly {
            Some(Aof::open(&config.aof_path())?)
        } else {
            None
        };

        let read_only = config.replicaof.is_some();
        Ok(Arc::new(Server {
            store,
            config,
            aof,
            repl: Replication::new(),
            read_only,
        }))
    }

    /// Start the background expiry reaper (samples and drops expired keys every 100ms).
    pub fn start_reaper(self: &Arc<Self>) {
        let server = Arc::clone(self);
        std::thread::Builder::new()
            .name("reaper".into())
            .spawn(move || loop {
                std::thread::sleep(Duration::from_millis(100));
                server.store.purge_expired();
            })
            .expect("spawn reaper");
    }

    /// If configured as a replica, start the background sync thread.
    pub fn start_replication(self: &Arc<Self>) {
        if self.read_only {
            crate::repl::start_replica(Arc::clone(self));
        }
    }

    /// Bind the configured address. Use port 0 to get an ephemeral port (handy for tests);
    /// read the real port back via `TcpListener::local_addr`.
    pub fn bind(&self) -> std::io::Result<TcpListener> {
        TcpListener::bind(format!("{}:{}", self.config.bind, self.config.port))
    }

    /// Bind and run the accept loop forever.
    pub fn serve(self: Arc<Self>) -> std::io::Result<()> {
        let listener = self.bind()?;
        let addr = listener.local_addr()?;
        eprintln!(
            "inmemd listening on {addr} — {} shards, maxmemory {}{}",
            self.config.shards,
            if self.config.maxmemory == 0 {
                "unlimited".to_string()
            } else {
                format!("{} bytes", self.config.maxmemory)
            },
            if self.config.appendonly {
                ", AOF on"
            } else {
                ""
            },
        );
        self.start_replication();
        self.run(listener)
    }

    /// Run the accept loop on an already-bound listener (one thread per connection).
    pub fn run(self: Arc<Self>, listener: TcpListener) -> std::io::Result<()> {
        let conn_ids = AtomicU64::new(1);
        for stream in listener.incoming() {
            match stream {
                Ok(stream) => {
                    let server = Arc::clone(&self);
                    let id = conn_ids.fetch_add(1, Ordering::Relaxed);
                    std::thread::Builder::new()
                        .name(format!("conn-{id}"))
                        .spawn(move || crate::conn::handle(stream, server, id))
                        .ok();
                }
                Err(e) => eprintln!("accept error: {e}"),
            }
        }
        Ok(())
    }
}
