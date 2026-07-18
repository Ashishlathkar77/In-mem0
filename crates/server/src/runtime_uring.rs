//! Linux io_uring thread-per-core runtime (ADR-002), built on glommio.
//!
//! Compiled only with `--features io-uring` on Linux. One pinned glommio executor per shard, each
//! with its own io_uring; `SO_REUSEPORT` (glommio's `TcpListener::bind` enables it) lets every
//! executor accept on the same port so the kernel load-balances connections — no shared accept
//! lock. The per-connection loop mirrors [`crate::conn`] but uses async io_uring reads/writes,
//! batching submit/complete to collapse the per-request syscall overhead that hurts `-P 1`.
//!
//! It reuses the existing synchronous pieces verbatim — [`parse_command_ranges`], [`dispatch`],
//! AOF, and replication propagation — so behavior matches the portable server. The single-owner
//! (lock-free) shard split described in ADR-002 D4 is a follow-up; this first cut keeps the
//! Mutex-sharded `Store` (correct, and already gets the io_uring I/O win). Replication `SYNC` is
//! served only by the portable listener, so run replicas against a plaintext portable build.

use crate::commands::{dispatch, ConnState};
use crate::repl::{encode, is_write_command};
use crate::server::Server;
use futures_lite::io::{AsyncReadExt, AsyncWriteExt};
use glommio::net::{TcpListener, TcpStream};
use glommio::{LocalExecutorPoolBuilder, PoolPlacement};
use inmem_proto::{parse_command_ranges, write_error};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

const READ_CHUNK: usize = 64 * 1024;
static CONN_IDS: AtomicU64 = AtomicU64::new(1);

/// Run the io_uring server: one pinned executor per CPU (NOT per shard — shards are store
/// partitions, independent of executor count), all accepting on the same port via SO_REUSEPORT.
pub fn serve(server: Arc<Server>) -> std::io::Result<()> {
    let executors = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(4);
    let addr = format!("{}:{}", server.config.bind, server.config.port);
    eprintln!(
        "inmemd (io_uring/glommio) listening on {addr} — {executors} executors, {} shards",
        server.config.shards
    );

    let handles = LocalExecutorPoolBuilder::new(PoolPlacement::MaxSpread(executors, None))
        .on_all_shards(move || {
            let server = server.clone();
            async move {
                if let Err(e) = accept_loop(server).await {
                    eprintln!("io_uring accept loop error: {e}");
                }
            }
        })
        .map_err(|e| std::io::Error::other(format!("glommio pool: {e:?}")))?;

    handles.join_all();
    Ok(())
}

async fn accept_loop(server: Arc<Server>) -> std::io::Result<()> {
    // glommio's bind sets SO_REUSEPORT, so every executor can bind the same address.
    let addr = format!("{}:{}", server.config.bind, server.config.port);
    let listener = TcpListener::bind(&addr).map_err(to_io)?;
    loop {
        match listener.accept().await {
            Ok(stream) => {
                let server = server.clone();
                let id = CONN_IDS.fetch_add(1, Ordering::Relaxed);
                glommio::spawn_local(async move {
                    let _ = handle_conn(server, stream, id).await;
                })
                .detach();
            }
            Err(e) => return Err(to_io(e)),
        }
    }
}

async fn handle_conn(server: Arc<Server>, mut stream: TcpStream, id: u64) -> std::io::Result<()> {
    stream.set_nodelay(true).ok();
    let authed = server.config.requirepass.is_none();
    let mut st = ConnState::new(id, authed);
    let mut inbuf: Vec<u8> = Vec::with_capacity(READ_CHUNK);
    let mut outbuf: Vec<u8> = Vec::with_capacity(READ_CHUNK);
    let mut ranges: Vec<(usize, usize)> = Vec::with_capacity(8);
    let mut chunk = vec![0u8; READ_CHUNK];

    loop {
        let n = stream.read(&mut chunk).await?;
        if n == 0 {
            return Ok(());
        }
        inbuf.extend_from_slice(&chunk[..n]);

        let mut cursor = 0;
        let mut quit = false;
        let mut batch_cmds: u64 = 0; // commands served this read; flushed once per read batch
        loop {
            let consumed = match parse_command_ranges(&inbuf[cursor..], &mut ranges) {
                Ok(Some(c)) => c,
                Ok(None) => break,
                Err(e) => {
                    write_error(&mut outbuf, &format!("ERR Protocol error: {e}"));
                    quit = true;
                    break;
                }
            };
            let base = cursor;
            cursor += consumed;
            if ranges.is_empty() {
                continue;
            }
            let argv: smallvec::SmallVec<[&[u8]; 16]> = ranges
                .iter()
                .map(|&(o, l)| &inbuf[base + o..base + o + l])
                .collect();

            let mut ub = [0u8; 24];
            let nlen = argv[0].len().min(ub.len());
            ub[..nlen].copy_from_slice(&argv[0][..nlen]);
            ub[..nlen].make_ascii_uppercase();
            let name = &ub[..nlen];

            if name == b"SYNC" || name == b"PSYNC" {
                write_error(
                    &mut outbuf,
                    "ERR replication SYNC is served by the portable listener",
                );
                quit = true;
                break;
            }
            if !st.authenticated && !matches!(name, b"AUTH" | b"HELLO" | b"QUIT" | b"PING") {
                write_error(&mut outbuf, "NOAUTH Authentication required.");
                continue;
            }
            if server.read_only && is_write_command(name) {
                write_error(
                    &mut outbuf,
                    "READONLY You can't write against a read only replica.",
                );
                continue;
            }

            // Zero-copy fast path (borrowed GET, alloc-free SET/INCR) shared with the portable
            // server; fall back to the full dispatcher for everything else.
            batch_cmds += 1;
            let is_write = match crate::conn::serve_fast(&server, &st, &argv, &mut outbuf) {
                Some(w) => w,
                None => {
                    let outcome = dispatch(&server.store, &argv, &mut st, &server.config);
                    outcome.reply.encode(&mut outbuf, st.resp3);
                    if outcome.quit {
                        quit = true;
                    }
                    outcome.persist
                }
            };
            if is_write && (server.aof.is_some() || server.repl.has_replicas()) {
                if let Some(aof) = &server.aof {
                    if let Err(e) = aof.append(&argv) {
                        write_error(&mut outbuf, &format!("ERR aof write failed: {e}"));
                        quit = true;
                    }
                }
                if server.repl.has_replicas() {
                    let mut bytes = Vec::with_capacity(32);
                    encode(&mut bytes, &argv);
                    server.repl.propagate(&bytes);
                }
            }
            if quit {
                break;
            }
        }
        server.store.add_commands(batch_cmds); // one atomic add per read batch, not per command

        if cursor > 0 {
            inbuf.drain(0..cursor);
        }
        if !outbuf.is_empty() {
            stream.write_all(&outbuf).await?;
            outbuf.clear();
        }
        if quit {
            return Ok(());
        }
    }
}

fn to_io(e: glommio::GlommioError<()>) -> std::io::Error {
    std::io::Error::other(format!("{e:?}"))
}
