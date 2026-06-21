//! Per-connection request/response loop.
//!
//! Threading model (v1): one OS thread per connection, all sharing the lock-sharded [`Store`].
//! Connections to independent keys never contend (different shards), and pipelined requests are
//! batched into a single write. This is simple, portable, and correct; the Linux thread-per-core
//! + io_uring upgrade (ADR-001 D3/D4) is a drop-in replacement for this module.
//!
//! Hot path (phase 5): commands are parsed **zero-copy** — arguments are `(offset, len)` ranges
//! into the read buffer, read as borrowed slices with no per-argument allocation. The hottest
//! commands (GET/SET/PING/INCR/DECR) are served inline, with `GET` encoding the stored value
//! straight into the output buffer via [`Store::read`] (no value copy). Everything else falls
//! back to the owned-argv [`dispatch`] path.

use crate::commands::{dispatch, ConnState};
use crate::server::Server;
use inmem_core::SetOptions;
use inmem_proto::{
    parse_command_ranges, write_bulk, write_error, write_int, write_null, write_simple,
};
use std::io::{Read, Write};
use std::net::TcpStream;
use std::sync::Arc;

const READ_CHUNK: usize = 64 * 1024;

pub fn handle(stream: TcpStream, server: Arc<Server>, id: u64) {
    // TCP_NODELAY matters for latency: Nagle's algorithm would otherwise delay small replies.
    let _ = stream.set_nodelay(true);
    if let Err(e) = run(stream, &server, id) {
        let _ = e; // routine: client disconnects show up here
    }
}

/// ASCII-uppercase a short command name into a stack buffer for matching, avoiding a heap
/// allocation per command. Returns the uppercased bytes (truncated to the buffer if longer).
fn upper<'a>(name: &[u8], buf: &'a mut [u8; 24]) -> &'a [u8] {
    let n = name.len().min(buf.len());
    for i in 0..n {
        buf[i] = name[i].to_ascii_uppercase();
    }
    &buf[..n]
}

fn run(mut stream: TcpStream, server: &Arc<Server>, id: u64) -> std::io::Result<()> {
    let mut st = ConnState {
        resp3: false,
        name: Vec::new(),
        id,
    };
    let mut inbuf: Vec<u8> = Vec::with_capacity(READ_CHUNK);
    let mut outbuf: Vec<u8> = Vec::with_capacity(READ_CHUNK);
    let mut ranges: Vec<(usize, usize)> = Vec::with_capacity(8);
    let mut chunk = [0u8; READ_CHUNK];

    loop {
        let n = stream.read(&mut chunk)?;
        if n == 0 {
            return Ok(()); // client closed
        }
        inbuf.extend_from_slice(&chunk[..n]);

        let mut cursor = 0;
        let mut quit = false;
        loop {
            let consumed = match parse_command_ranges(&inbuf[cursor..], &mut ranges) {
                Ok(Some(c)) => c,
                Ok(None) => break, // need more bytes
                Err(e) => {
                    write_error(&mut outbuf, &format!("ERR Protocol error: {e}"));
                    quit = true;
                    break;
                }
            };
            let base = cursor;
            cursor += consumed;
            if ranges.is_empty() {
                continue; // empty/null command
            }

            // Borrowed argument slices into `inbuf` (no copies).
            let argv: Vec<&[u8]> = ranges
                .iter()
                .map(|&(o, l)| &inbuf[base + o..base + o + l])
                .collect();

            if !serve_fast(server, &st, &argv, &mut outbuf, &mut quit)? {
                // Cold path: own the args and run the full dispatcher.
                let owned: Vec<Vec<u8>> = argv.iter().map(|a| a.to_vec()).collect();
                let outcome = dispatch(&server.store, &owned, &mut st, &server.config);
                if outcome.persist {
                    if let Some(aof) = &server.aof {
                        if let Err(e) = aof.append(&argv) {
                            write_error(&mut outbuf, &format!("ERR aof write failed: {e}"));
                            quit = true;
                            break;
                        }
                    }
                }
                outcome.reply.encode(&mut outbuf, st.resp3);
                if outcome.quit {
                    quit = true;
                    break;
                }
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

/// Try to serve the command on the zero-copy fast path. Returns `Ok(true)` if handled (reply
/// written to `out`), `Ok(false)` if the caller should use the cold dispatch path.
fn serve_fast(
    server: &Arc<Server>,
    st: &ConnState,
    argv: &[&[u8]],
    out: &mut Vec<u8>,
    quit: &mut bool,
) -> std::io::Result<bool> {
    let mut ubuf = [0u8; 24];
    let name = upper(argv[0], &mut ubuf);
    let store = &server.store;

    match name {
        b"GET" if argv.len() == 2 => {
            store.read(argv[1], |v| match v {
                Some(b) => write_bulk(out, b),
                None => write_null(out, st.resp3),
            });
            Ok(true)
        }
        b"SET" if argv.len() == 3 => {
            // simple SET (no options) — the benchmark-dominant write
            store.set(argv[1], argv[2], SetOptions::default());
            if let Some(aof) = &server.aof {
                if aof.append(argv).is_err() {
                    write_error(out, "ERR aof write failed");
                    *quit = true;
                    return Ok(true);
                }
            }
            write_simple(out, "OK");
            Ok(true)
        }
        b"PING" if argv.len() == 1 => {
            write_simple(out, "PONG");
            Ok(true)
        }
        b"INCR" if argv.len() == 2 => fast_incr(server, argv, 1, out),
        b"DECR" if argv.len() == 2 => fast_incr(server, argv, -1, out),
        _ => Ok(false),
    }
}

fn fast_incr(
    server: &Arc<Server>,
    argv: &[&[u8]],
    delta: i64,
    out: &mut Vec<u8>,
) -> std::io::Result<bool> {
    match server.store.incr_by(argv[1], delta) {
        Ok(v) => {
            if let Some(aof) = &server.aof {
                if aof.append(argv).is_err() {
                    write_error(out, "ERR aof write failed");
                    return Ok(true);
                }
            }
            write_int(out, v);
        }
        Err(e) => write_error(out, &format!("ERR {e}")),
    }
    Ok(true)
}
