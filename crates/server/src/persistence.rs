//! Durability: append-only file (AOF) and snapshots (ADR-001 D8).
//!
//! - **AOF**: every keyspace-mutating command is appended as its RESP encoding. On startup the
//!   log is replayed to reconstruct state.
//! - **Snapshot**: a stream of RESP commands that recreate the dataset (type-generic across
//!   strings/lists/hashes/sets/sorted-sets + TTLs), replayed through the same path as the AOF.
//!
//! Current fsync policy is flush-on-append (durable to the OS page cache each write). A periodic
//! `fsync` ("everysec") and fork-COW background snapshotting are future work — see ADR-001 D8.

use crate::commands::{dispatch, ConnState};
use crate::config::Config;
use inmem_core::Store;
use inmem_proto::parse_command;
use std::fs::{File, OpenOptions};
use std::io::{self, BufWriter, Read, Write};
use std::path::Path;
use std::sync::Mutex;

/// Append-only file handle, shared across connection threads.
pub struct Aof {
    writer: Mutex<BufWriter<File>>,
}

impl Aof {
    /// Open (creating if needed) the AOF for appending.
    pub fn open(path: &Path) -> io::Result<Aof> {
        let file = OpenOptions::new().create(true).append(true).open(path)?;
        Ok(Aof {
            writer: Mutex::new(BufWriter::new(file)),
        })
    }

    /// Append one command. Flushes to the OS so a crash loses at most in-flight buffered bytes.
    pub fn append(&self, argv: &[&[u8]]) -> io::Result<()> {
        let mut buf = Vec::with_capacity(32);
        encode_command(argv, &mut buf);
        let mut w = self.writer.lock().unwrap();
        w.write_all(&buf)?;
        w.flush()
    }
}

/// Encode an argument vector as a RESP array of bulk strings (the AOF on-disk form).
fn encode_command(argv: &[&[u8]], out: &mut Vec<u8>) {
    out.push(b'*');
    out.extend_from_slice(argv.len().to_string().as_bytes());
    out.extend_from_slice(b"\r\n");
    for a in argv {
        out.push(b'$');
        out.extend_from_slice(a.len().to_string().as_bytes());
        out.extend_from_slice(b"\r\n");
        out.extend_from_slice(a);
        out.extend_from_slice(b"\r\n");
    }
}

/// Replay an AOF into `store`. Returns the number of commands applied.
pub fn load_aof(path: &Path, store: &Store, cfg: &Config) -> io::Result<usize> {
    replay_file(store, path, cfg)
}

/// Replay a file of RESP commands (AOF or snapshot) into the store. Returns commands applied.
/// A truncated or corrupt tail (e.g. a partial write before a crash) stops replay cleanly at the
/// last good command rather than erroring.
fn replay_file(store: &Store, path: &Path, cfg: &Config) -> io::Result<usize> {
    let mut data = Vec::new();
    match File::open(path) {
        Ok(mut f) => f.read_to_end(&mut data)?,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(0),
        Err(e) => return Err(e),
    };
    let mut st = ConnState::internal();
    let mut cursor = 0;
    let mut applied = 0;
    while cursor < data.len() {
        match parse_command(&data[cursor..]) {
            Ok(Some((argv, consumed))) => {
                cursor += consumed;
                if !argv.is_empty() {
                    let owned: Vec<&[u8]> = argv.iter().map(|a| a.as_slice()).collect();
                    let _ = dispatch(store, &owned, &mut st, cfg);
                    applied += 1;
                }
            }
            Ok(None) => break,
            Err(_) => break,
        }
    }
    Ok(applied)
}

/// Write a snapshot of all live entries to `path` (atomic via temp file + rename).
///
/// The snapshot is a stream of RESP commands that recreate the dataset (type-generic — works for
/// strings, lists, hashes, sets, sorted sets, and TTLs), so it replays through the same path as
/// the AOF.
pub fn save_snapshot(store: &Store, path: &Path) -> io::Result<()> {
    let tmp = path.with_extension("tmp");
    {
        let mut w = BufWriter::new(File::create(&tmp)?);
        w.write_all(&store.dump_commands())?;
        w.flush()?;
    }
    std::fs::rename(&tmp, path)
}

/// Load a snapshot into `store` by replaying its commands. Returns number of commands applied.
pub fn load_snapshot(store: &Store, path: &Path, cfg: &Config) -> io::Result<usize> {
    replay_file(store, path, cfg)
}

#[cfg(test)]
mod tests {
    use super::*;
    use inmem_core::SetOptions;

    fn tmp_path(name: &str) -> std::path::PathBuf {
        let mut p = std::env::temp_dir();
        // unique-ish per test name to avoid collisions in parallel runs
        p.push(format!("inmem-test-{}-{}", std::process::id(), name));
        p
    }

    #[test]
    fn snapshot_roundtrip() {
        let path = tmp_path("snap");
        let st = Store::new(4, 0);
        st.set(b"a", b"1", SetOptions::default());
        st.set(
            b"b",
            b"two",
            SetOptions {
                expire_at: Some(inmem_core::now_ms() + 100_000),
                ..Default::default()
            },
        );
        save_snapshot(&st, &path).unwrap();

        let st2 = Store::new(4, 0);
        let n = load_snapshot(&st2, &path, &Config::default()).unwrap();
        assert_eq!(n, 3); // SET a, SET b, PEXPIREAT b
        assert_eq!(st2.get(b"a").unwrap().as_deref(), Some(&b"1"[..]));
        assert_eq!(st2.get(b"b").unwrap().as_deref(), Some(&b"two"[..]));
        assert!(matches!(st2.pttl(b"b"), inmem_core::Ttl::Millis(_)));
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn snapshot_roundtrips_all_types() {
        let path = tmp_path("snap-types");
        let st = Store::new(4, 0);
        st.set(b"s", b"v", SetOptions::default());
        st.push(b"l", &[b"x", b"y"], false).unwrap();
        st.hset(b"h", &[(b"a", b"1")]).unwrap();
        st.sadd(b"set", &[b"m"]).unwrap();
        st.zadd(b"z", &[(1.5, b"m")]).unwrap();
        save_snapshot(&st, &path).unwrap();

        let st2 = Store::new(4, 0);
        load_snapshot(&st2, &path, &Config::default()).unwrap();
        assert_eq!(st2.get(b"s").unwrap().as_deref(), Some(&b"v"[..]));
        assert_eq!(st2.llen(b"l"), Ok(2));
        assert_eq!(st2.hget(b"h", b"a").unwrap().as_deref(), Some(&b"1"[..]));
        assert_eq!(st2.sismember(b"set", b"m"), Ok(true));
        assert_eq!(st2.zscore(b"z", b"m").unwrap(), Some(1.5));
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn aof_roundtrip() {
        let path = tmp_path("aof");
        std::fs::remove_file(&path).ok();
        let cfg = Config::default();
        {
            let aof = Aof::open(&path).unwrap();
            aof.append(&[b"SET".as_ref(), b"x".as_ref(), b"1".as_ref()])
                .unwrap();
            aof.append(&[b"INCR".as_ref(), b"x".as_ref()]).unwrap();
            aof.append(&[b"SET".as_ref(), b"y".as_ref(), b"hi".as_ref()])
                .unwrap();
        }
        let st = Store::new(4, 0);
        let applied = load_aof(&path, &st, &cfg).unwrap();
        assert_eq!(applied, 3);
        assert_eq!(st.get(b"x").unwrap().as_deref(), Some(&b"2"[..]));
        assert_eq!(st.get(b"y").unwrap().as_deref(), Some(&b"hi"[..]));
        std::fs::remove_file(&path).ok();
    }
}
