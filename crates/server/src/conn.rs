//! Per-connection request/response loop.
//!
//! Threading model (v1): one OS thread per connection, all sharing the lock-sharded [`Store`].
//! Connections to independent keys never contend (different shards), and pipelined requests are
//! batched into a single write. The Linux thread-per-core + io_uring upgrade (ADR-001 D3/D4) is a
//! drop-in replacement for this module.
//!
//! Hot path: commands are parsed **zero-copy** (arguments are `(offset,len)` ranges into the read
//! buffer). The hottest commands (GET/SET/PING/INCR/DECR) are served inline, with `GET` encoding
//! the stored value straight into the output buffer via [`Store::read_str`] (no value copy).
//! Everything else goes through the full [`dispatch`].

use crate::commands::{dispatch, ConnState};
use crate::repl::{encode, is_write_command};
use crate::server::Server;
use inmem_core::{SetOptions, StrRead};
use inmem_proto::{
    parse_command_ranges, write_bulk, write_error, write_int, write_null, write_simple,
};
use std::io::{Read, Write};
use std::net::TcpStream;
use std::sync::Arc;

const READ_CHUNK: usize = 64 * 1024;

pub fn handle(stream: TcpStream, server: Arc<Server>, id: u64) {
    let _ = stream.set_nodelay(true);

    // With TLS configured, wrap the socket; replication SYNC is only offered on plaintext.
    #[cfg(feature = "tls")]
    if let Some(acceptor) = server.tls.clone() {
        match acceptor.accept(stream) {
            Ok(tls_stream) => {
                let _ = run(tls_stream, &server, id, None);
            }
            Err(e) => eprintln!("tls handshake failed: {e}"),
        }
        return;
    }

    let raw_for_sync = stream.try_clone().ok();
    if let Err(e) = run(stream, &server, id, raw_for_sync) {
        let _ = e; // routine: client disconnects show up here
    }
}

fn upper<'a>(name: &[u8], buf: &'a mut [u8; 24]) -> &'a [u8] {
    let n = name.len().min(buf.len());
    buf[..n].copy_from_slice(&name[..n]);
    buf[..n].make_ascii_uppercase();
    &buf[..n]
}

/// Drive one connection. Generic over the stream so a plain `TcpStream` or a TLS-wrapped stream
/// both work. `raw_for_sync` is a cloneable TCP handle used only to register a replica feed on
/// `SYNC` (present for plaintext connections, `None` over TLS).
fn run<S: Read + Write>(
    mut stream: S,
    server: &Arc<Server>,
    id: u64,
    raw_for_sync: Option<TcpStream>,
) -> std::io::Result<()> {
    let authed = server.config.requirepass.is_none();
    let mut st = ConnState::new(id, authed);
    let mut inbuf: Vec<u8> = Vec::with_capacity(READ_CHUNK);
    let mut outbuf: Vec<u8> = Vec::with_capacity(READ_CHUNK);
    let mut ranges: Vec<(usize, usize)> = Vec::with_capacity(8);
    let mut chunk = [0u8; READ_CHUNK];

    loop {
        let n = stream.read(&mut chunk)?;
        if n == 0 {
            return Ok(());
        }
        inbuf.extend_from_slice(&chunk[..n]);

        let mut cursor = 0;
        let mut quit = false;
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

            let mut ubuf = [0u8; 24];
            let name = upper(argv[0], &mut ubuf).to_vec();

            // A replica subscribing for the write stream: send the snapshot, register, go passive.
            if name == b"SYNC" || name == b"PSYNC" {
                if !outbuf.is_empty() {
                    stream.write_all(&outbuf)?;
                    outbuf.clear();
                }
                match raw_for_sync {
                    Some(raw) => {
                        let snapshot = server.store.dump_commands();
                        if let Ok(clone) = raw.try_clone() {
                            server.repl.add_replica_with_snapshot(clone, &snapshot);
                        }
                        return passive_replica(stream);
                    }
                    None => {
                        write_error(&mut outbuf, "ERR replication is not supported over TLS");
                        stream.write_all(&outbuf)?;
                        return Ok(());
                    }
                }
            }

            // Auth gate: when a password is set, only AUTH/HELLO/QUIT/PING run unauthenticated.
            if !st.authenticated && !is_preauth_ok(&name) {
                write_error(&mut outbuf, "NOAUTH Authentication required.");
                continue;
            }
            // Read-only replica: reject client writes.
            if server.read_only && is_write_command(&name) {
                write_error(
                    &mut outbuf,
                    "READONLY You can't write against a read only replica.",
                );
                continue;
            }

            let handled = serve_fast(server, &st, &argv, &mut outbuf);
            let is_write = match handled {
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

            if is_write {
                // Durability + replication: encode once, append to AOF, fan out to replicas.
                let mut bytes = Vec::with_capacity(32);
                encode(&mut bytes, &argv);
                if let Some(aof) = &server.aof {
                    if let Err(e) = aof.append(&argv) {
                        write_error(&mut outbuf, &format!("ERR aof write failed: {e}"));
                        quit = true;
                    }
                }
                server.repl.propagate(&bytes);
            }

            if quit {
                break;
            }
        }

        if cursor > 0 {
            inbuf.drain(0..cursor);
        }
        if !outbuf.is_empty() {
            stream.write_all(&outbuf)?;
            outbuf.clear();
        }
        if quit {
            return Ok(());
        }
    }
}

/// After `SYNC`, the connection becomes a one-way write feed: the replication registry owns a
/// clone for pushing commands, and this side just drains anything the replica sends (e.g. ACKs)
/// until it disconnects.
fn passive_replica<S: Read>(mut stream: S) -> std::io::Result<()> {
    let mut chunk = [0u8; 4096];
    loop {
        if stream.read(&mut chunk)? == 0 {
            return Ok(());
        }
    }
}

fn is_preauth_ok(name_upper: &[u8]) -> bool {
    matches!(name_upper, b"AUTH" | b"HELLO" | b"QUIT" | b"PING")
}

/// Try to serve on the zero-copy fast path. `Some(is_write)` if handled (reply already written);
/// `None` to fall back to [`dispatch`]. Does not handle AOF/replication — the caller does.
/// Shared with the io_uring runtime so both transports get the borrowed-GET / alloc-free path.
pub(crate) fn serve_fast(
    server: &Arc<Server>,
    st: &ConnState,
    argv: &[&[u8]],
    out: &mut Vec<u8>,
) -> Option<bool> {
    let mut ubuf = [0u8; 24];
    let name = upper(argv[0], &mut ubuf);
    let store = &server.store;

    match name {
        b"GET" if argv.len() == 2 => {
            store.read_str(argv[1], |r| match r {
                StrRead::Str(b) => write_bulk(out, b),
                StrRead::None => write_null(out, st.resp3),
                StrRead::WrongType => write_error(out, inmem_core::WRONGTYPE),
            });
            Some(false)
        }
        b"SET" if argv.len() == 3 => {
            store.set(argv[1], argv[2], SetOptions::default());
            write_simple(out, "OK");
            Some(true)
        }
        b"PING" if argv.len() == 1 => {
            write_simple(out, "PONG");
            Some(false)
        }
        b"INCR" if argv.len() == 2 => Some(fast_incr(store, argv[1], 1, out)),
        b"DECR" if argv.len() == 2 => Some(fast_incr(store, argv[1], -1, out)),
        _ => None,
    }
}

/// Returns whether the operation was a (successful) write that should be propagated.
fn fast_incr(store: &inmem_core::Store, key: &[u8], delta: i64, out: &mut Vec<u8>) -> bool {
    match store.incr_by(key, delta) {
        Ok(v) => {
            write_int(out, v);
            true
        }
        Err(e) => {
            if e == inmem_core::WRONGTYPE {
                write_error(out, e);
            } else {
                write_error(out, &format!("ERR {e}"));
            }
            false
        }
    }
}
