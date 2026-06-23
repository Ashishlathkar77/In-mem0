//! Asynchronous primary/replica replication.
//!
//! **Primary**: when a connection issues `SYNC`/`PSYNC`, the primary streams a full snapshot
//! (a sequence of RESP reconstruction commands) and then registers the socket to receive every
//! subsequent write command. Propagation is best-effort and asynchronous.
//!
//! **Replica** (`--replicaof host:port`): connects to the primary, optionally `AUTH`s, sends
//! `SYNC`, then applies the incoming command stream to its local store forever (reconnecting on
//! failure). A replica is read-only to normal clients.
//!
//! This is a pragmatic v1: there is no replication backlog/offset, so a write that races the
//! initial handshake could be applied twice on a replica. Steady-state propagation after the
//! handshake is exactly-once. Redis-compatible PSYNC/partial resync is future work.

use crate::commands::{dispatch, ConnState};
use crate::config::Config;
use crate::server::Server;
use inmem_core::Store;
use inmem_proto::parse_command;
use std::io::{Read, Write};
use std::net::TcpStream;
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// Registry of connected replica sockets on a primary.
#[derive(Default)]
pub struct Replication {
    replicas: Mutex<Vec<TcpStream>>,
    /// Lock-free replica count so the hot write path can skip command encoding/propagation
    /// entirely when there are no replicas (the common case).
    count: std::sync::atomic::AtomicUsize,
}

impl Replication {
    pub fn new() -> Self {
        Replication::default()
    }

    /// Cheap (lock-free) check used on the hot path.
    #[inline]
    pub fn has_replicas(&self) -> bool {
        self.count.load(std::sync::atomic::Ordering::Relaxed) > 0
    }

    pub fn replica_count(&self) -> usize {
        self.count.load(std::sync::atomic::Ordering::Relaxed)
    }

    /// Register a replica and send it the initial snapshot atomically (under the registry lock,
    /// so no propagated write can interleave with the snapshot bytes on the wire).
    pub fn add_replica_with_snapshot(&self, mut sock: TcpStream, snapshot: &[u8]) {
        let mut guard = self.replicas.lock().unwrap();
        if sock.write_all(snapshot).is_ok() {
            guard.push(sock);
            self.count
                .store(guard.len(), std::sync::atomic::Ordering::Relaxed);
        }
    }

    /// Propagate raw command bytes to all replicas, dropping any that error.
    pub fn propagate(&self, bytes: &[u8]) {
        let mut v = self.replicas.lock().unwrap();
        if v.is_empty() {
            return;
        }
        v.retain_mut(|s| s.write_all(bytes).is_ok());
        self.count
            .store(v.len(), std::sync::atomic::Ordering::Relaxed);
    }
}

/// Spawn the replica client thread: connect to the primary, sync, and apply the stream forever.
pub fn start_replica(server: Arc<Server>) {
    let Some((host, port)) = server.config.replicaof.clone() else {
        return;
    };
    std::thread::Builder::new()
        .name("replica".into())
        .spawn(move || loop {
            match sync_once(&server.store, &server.config, &host, port) {
                Ok(()) => eprintln!("replica: primary {host}:{port} closed connection"),
                Err(e) => eprintln!("replica: sync to {host}:{port} failed: {e}"),
            }
            std::thread::sleep(Duration::from_secs(1)); // backoff before reconnect
        })
        .expect("spawn replica thread");
}

fn sync_once(store: &Store, cfg: &Config, host: &str, port: u16) -> std::io::Result<()> {
    let mut sock = TcpStream::connect((host, port))?;
    sock.set_nodelay(true).ok();

    if let Some(pass) = &cfg.masterauth {
        let mut auth = Vec::new();
        encode(&mut auth, &[b"AUTH", pass.as_bytes()]);
        sock.write_all(&auth)?;
        // swallow the +OK (best-effort)
        let mut tmp = [0u8; 64];
        let _ = sock.read(&mut tmp)?;
    }

    sock.write_all(b"*1\r\n$4\r\nSYNC\r\n")?;
    eprintln!("replica: synced to primary {host}:{port}, applying stream");

    let mut st = ConnState::internal();
    let mut buf: Vec<u8> = Vec::with_capacity(64 * 1024);
    let mut chunk = [0u8; 64 * 1024];
    loop {
        let n = sock.read(&mut chunk)?;
        if n == 0 {
            return Ok(());
        }
        buf.extend_from_slice(&chunk[..n]);
        let mut cursor = 0;
        loop {
            match parse_command(&buf[cursor..]) {
                Ok(Some((argv, consumed))) => {
                    cursor += consumed;
                    if !argv.is_empty() {
                        let borrowed: Vec<&[u8]> = argv.iter().map(|a| a.as_slice()).collect();
                        let _ = dispatch(store, &borrowed, &mut st, cfg);
                    }
                }
                Ok(None) => break,
                Err(_) => return Ok(()), // corrupt stream; reconnect
            }
        }
        if cursor > 0 {
            buf.drain(0..cursor);
        }
    }
}

/// Encode a command as a RESP array of bulk strings.
pub fn encode(out: &mut Vec<u8>, args: &[&[u8]]) {
    out.push(b'*');
    out.extend_from_slice(args.len().to_string().as_bytes());
    out.extend_from_slice(b"\r\n");
    for a in args {
        out.push(b'$');
        out.extend_from_slice(a.len().to_string().as_bytes());
        out.extend_from_slice(b"\r\n");
        out.extend_from_slice(a);
        out.extend_from_slice(b"\r\n");
    }
}

/// Is this command a write (mutates the keyspace)? Used to reject writes on a read-only replica.
pub fn is_write_command(name_upper: &[u8]) -> bool {
    matches!(
        name_upper,
        b"SET"
            | b"SETNX"
            | b"SETEX"
            | b"PSETEX"
            | b"GETSET"
            | b"MSET"
            | b"APPEND"
            | b"INCR"
            | b"DECR"
            | b"INCRBY"
            | b"DECRBY"
            | b"DEL"
            | b"UNLINK"
            | b"EXPIRE"
            | b"PEXPIRE"
            | b"EXPIREAT"
            | b"PEXPIREAT"
            | b"PERSIST"
            | b"FLUSHALL"
            | b"FLUSHDB"
            | b"LPUSH"
            | b"RPUSH"
            | b"LPOP"
            | b"RPOP"
            | b"HSET"
            | b"HMSET"
            | b"HDEL"
            | b"SADD"
            | b"SREM"
            | b"ZADD"
            | b"ZREM"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encodes_command() {
        let mut o = Vec::new();
        encode(&mut o, &[b"SET", b"k", b"v"]);
        assert_eq!(o, b"*3\r\n$3\r\nSET\r\n$1\r\nk\r\n$1\r\nv\r\n");
    }

    #[test]
    fn write_classification() {
        assert!(is_write_command(b"SET"));
        assert!(is_write_command(b"LPUSH"));
        assert!(!is_write_command(b"GET"));
        assert!(!is_write_command(b"PING"));
    }
}
