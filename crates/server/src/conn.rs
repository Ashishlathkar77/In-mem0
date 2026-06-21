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
    if let Err(e) = run(stream, &server, id) {
        let _ = e; // routine: client disconnects show up here
    }
}

fn upper<'a>(name: &[u8], buf: &'a mut [u8; 24]) -> &'a [u8] {
    let n = name.len().min(buf.len());
    buf[..n].copy_from_slice(&name[..n]);
    buf[..n].make_ascii_uppercase();
    &buf[..n]
}

fn run(mut stream: TcpStream, server: &Arc<Server>, id: u64) -> std::io::Result<()> {
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
            let argv: Vec<&[u8]> = ranges
                .iter()
                .map(|&(o, l)| &inbuf[base + o..base + o + l])
                .collect();

            // Auth gate: when a password is set, only AUTH/HELLO/QUIT/PING are allowed first.
            if !st.authenticated && !is_preauth_ok(argv[0]) {
                write_error(&mut outbuf, "NOAUTH Authentication required.");
                continue;
            }

            if !serve_fast(server, &st, &argv, &mut outbuf, &mut quit)? {
                let outcome = dispatch(&server.store, &argv, &mut st, &server.config);
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

/// Commands permitted before authentication.
fn is_preauth_ok(cmd: &[u8]) -> bool {
    let mut buf = [0u8; 24];
    matches!(upper(cmd, &mut buf), b"AUTH" | b"HELLO" | b"QUIT" | b"PING")
}

/// Try to serve on the zero-copy fast path. Returns Ok(true) if handled.
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
            store.read_str(argv[1], |r| match r {
                StrRead::Str(b) => write_bulk(out, b),
                StrRead::None => write_null(out, st.resp3),
                StrRead::WrongType => write_error(
                    out,
                    "WRONGTYPE Operation against a key holding the wrong kind of value",
                ),
            });
            Ok(true)
        }
        b"SET" if argv.len() == 3 => {
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
        Err(e) => {
            if e == inmem_core::WRONGTYPE {
                write_error(out, e);
            } else {
                write_error(out, &format!("ERR {e}"));
            }
        }
    }
    Ok(true)
}
