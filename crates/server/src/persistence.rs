//! Durability: append-only file (AOF) and binary snapshots (ADR-001 D8).
//!
//! - **AOF**: every keyspace-mutating command is appended as its RESP encoding. On startup the
//!   log is replayed to reconstruct state. This is the simple, robust durability path.
//! - **Snapshot**: a compact length-prefixed dump of all live `(key, value, expire_at)` triples,
//!   written by `SAVE`/`BGSAVE` and loadable at startup.
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

const SNAPSHOT_MAGIC: &[u8] = b"INMEMSNP1";

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
    let mut data = Vec::new();
    match File::open(path) {
        Ok(mut f) => f.read_to_end(&mut data)?,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(0),
        Err(e) => return Err(e),
    };
    let mut st = ConnState {
        resp3: false,
        name: Vec::new(),
        id: 0,
    };
    let mut cursor = 0;
    let mut applied = 0;
    while cursor < data.len() {
        match parse_command(&data[cursor..]) {
            Ok(Some((argv, consumed))) => {
                cursor += consumed;
                if !argv.is_empty() {
                    // Replay against the store; we deliberately ignore the persist flag here.
                    let _ = dispatch(store, &argv, &mut st, cfg);
                    applied += 1;
                }
            }
            Ok(None) => break, // truncated tail (partial write before a crash) — stop cleanly
            Err(_) => break,   // corrupt tail — stop at the last good command
        }
    }
    Ok(applied)
}

/// Write a snapshot of all live entries to `path` (atomic via temp file + rename).
pub fn save_snapshot(store: &Store, path: &Path) -> io::Result<()> {
    let tmp = path.with_extension("tmp");
    {
        let mut w = BufWriter::new(File::create(&tmp)?);
        w.write_all(SNAPSHOT_MAGIC)?;
        for (k, v, expire) in store.snapshot() {
            w.write_all(&(k.len() as u32).to_le_bytes())?;
            w.write_all(&k)?;
            w.write_all(&(v.len() as u32).to_le_bytes())?;
            w.write_all(&v)?;
            match expire {
                Some(t) => {
                    w.write_all(&[1u8])?;
                    w.write_all(&t.to_le_bytes())?;
                }
                None => w.write_all(&[0u8])?,
            }
        }
        w.flush()?;
    }
    std::fs::rename(&tmp, path)
}

/// Load a snapshot into `store`. Returns number of keys loaded (0 if file absent).
pub fn load_snapshot(store: &Store, path: &Path) -> io::Result<usize> {
    let mut data = Vec::new();
    match File::open(path) {
        Ok(mut f) => f.read_to_end(&mut data)?,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(0),
        Err(e) => return Err(e),
    };
    if !data.starts_with(SNAPSHOT_MAGIC) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "bad snapshot magic",
        ));
    }
    let mut p = SNAPSHOT_MAGIC.len();
    let mut n = 0;
    let read_u32 = |data: &[u8], p: &mut usize| -> Option<usize> {
        if *p + 4 > data.len() {
            return None;
        }
        let v = u32::from_le_bytes(data[*p..*p + 4].try_into().unwrap()) as usize;
        *p += 4;
        Some(v)
    };
    while p < data.len() {
        let Some(klen) = read_u32(&data, &mut p) else {
            break;
        };
        if p + klen > data.len() {
            break;
        }
        let key = data[p..p + klen].to_vec();
        p += klen;
        let Some(vlen) = read_u32(&data, &mut p) else {
            break;
        };
        if p + vlen > data.len() {
            break;
        }
        let val = data[p..p + vlen].to_vec();
        p += vlen;
        if p >= data.len() {
            break;
        }
        let has_exp = data[p];
        p += 1;
        let expire = if has_exp == 1 {
            if p + 8 > data.len() {
                break;
            }
            let t = u64::from_le_bytes(data[p..p + 8].try_into().unwrap());
            p += 8;
            Some(t)
        } else {
            None
        };
        store.set(
            &key,
            &val,
            inmem_core::SetOptions {
                expire_at: expire,
                ..Default::default()
            },
        );
        n += 1;
    }
    Ok(n)
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
        let n = load_snapshot(&st2, &path).unwrap();
        assert_eq!(n, 2);
        assert_eq!(st2.get(b"a").as_deref(), Some(&b"1"[..]));
        assert_eq!(st2.get(b"b").as_deref(), Some(&b"two"[..]));
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
        assert_eq!(st.get(b"x").as_deref(), Some(&b"2"[..]));
        assert_eq!(st.get(b"y").as_deref(), Some(&b"hi"[..]));
        std::fs::remove_file(&path).ok();
    }
}
