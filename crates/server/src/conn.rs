//! Per-connection request/response loop.
//!
//! Threading model (v1): one OS thread per connection, all sharing the lock-sharded [`Store`].
//! Connections to independent keys never contend (different shards), and pipelined requests are
//! batched into a single write. This is simple, portable, and correct; the Linux thread-per-core
//! + io_uring upgrade (ADR-001 D3/D4) is a drop-in replacement for this module.

use crate::commands::{dispatch, ConnState};
use crate::server::Server;
use inmem_proto::{parse_command, Reply};
use std::io::{Read, Write};
use std::net::TcpStream;
use std::sync::Arc;

const READ_CHUNK: usize = 64 * 1024;

pub fn handle(stream: TcpStream, server: Arc<Server>, id: u64) {
    // TCP_NODELAY matters for latency: Nagle's algorithm would otherwise delay small replies.
    let _ = stream.set_nodelay(true);
    if let Err(e) = run(stream, &server, id) {
        // Connection errors are routine (client disconnects); only log at debug-ish level.
        let _ = e;
    }
}

fn run(mut stream: TcpStream, server: &Arc<Server>, id: u64) -> std::io::Result<()> {
    let mut st = ConnState {
        resp3: false,
        name: Vec::new(),
        id,
    };
    let mut inbuf: Vec<u8> = Vec::with_capacity(READ_CHUNK);
    let mut outbuf: Vec<u8> = Vec::with_capacity(READ_CHUNK);
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
            match parse_command(&inbuf[cursor..]) {
                Ok(Some((argv, consumed))) => {
                    cursor += consumed;
                    if argv.is_empty() {
                        continue;
                    }
                    let outcome = dispatch(&server.store, &argv, &mut st, &server.config);
                    if outcome.persist {
                        if let Some(aof) = &server.aof {
                            // A failed disk write must not silently drop durability.
                            if let Err(e) = aof.append(&argv) {
                                let msg = format!("-ERR aof write failed: {e}\r\n");
                                outbuf.extend_from_slice(msg.as_bytes());
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
                Ok(None) => break, // need more bytes
                Err(e) => {
                    let mut r = Vec::new();
                    Reply::Error(format!("ERR Protocol error: {e}")).encode(&mut r, st.resp3);
                    outbuf.extend_from_slice(&r);
                    quit = true;
                    break;
                }
            }
        }

        // Drop consumed bytes; keep any partial trailing frame for the next read.
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
