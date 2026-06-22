//! Server configuration and command-line parsing.

use std::path::PathBuf;

#[derive(Clone, Debug)]
pub struct Config {
    pub bind: String,
    pub port: u16,
    pub shards: usize,
    /// Total memory budget in bytes; 0 = unbounded.
    pub maxmemory: usize,
    /// Enable append-only-file persistence.
    pub appendonly: bool,
    /// Optional password; when set, connections must `AUTH` before other commands.
    pub requirepass: Option<String>,
    /// If set, run as a read-only replica of this primary `(host, port)`.
    pub replicaof: Option<(String, u16)>,
    /// Password to use when authenticating to the primary (if it requires one).
    pub masterauth: Option<String>,
    /// TLS certificate chain (PEM). With `tls_key`, enables TLS (requires the `tls` feature).
    pub tls_cert: Option<PathBuf>,
    /// TLS private key (PEM).
    pub tls_key: Option<PathBuf>,
    /// Working directory for persistence files.
    pub dir: PathBuf,
    pub aof_file: String,
    pub snapshot_file: String,
}

/// Default store partition count. The store is sharded for concurrency, not pinned to core count
/// — more shards means lower lock contention among concurrent connections (benchmarks show a
/// large throughput gain going from ~16 to a few hundred shards). Shards are cheap when empty.
pub const DEFAULT_SHARDS: usize = 512;

impl Default for Config {
    fn default() -> Self {
        Config {
            bind: "127.0.0.1".into(),
            port: 6380, // not 6379, to avoid clashing with a local Redis
            shards: DEFAULT_SHARDS,
            maxmemory: 0,
            appendonly: false,
            requirepass: None,
            replicaof: None,
            masterauth: None,
            tls_cert: None,
            tls_key: None,
            dir: PathBuf::from("."),
            aof_file: "inmem.aof".into(),
            snapshot_file: "inmem.snapshot".into(),
        }
    }
}

impl Config {
    pub fn aof_path(&self) -> PathBuf {
        self.dir.join(&self.aof_file)
    }
    pub fn snapshot_path(&self) -> PathBuf {
        self.dir.join(&self.snapshot_file)
    }

    /// Parse `--flag value` style args (Redis-ish). Unknown flags are ignored with a warning.
    pub fn from_args(args: impl Iterator<Item = String>) -> Result<Config, String> {
        let mut cfg = Config::default();
        let mut it = args.peekable();
        while let Some(arg) = it.next() {
            let key = arg.trim_start_matches("--").to_ascii_lowercase();
            match key.as_str() {
                "help" | "h" => return Err("help".into()),
                "port" => cfg.port = need(&mut it, "port")?.parse().map_err(|_| "invalid port")?,
                "bind" => cfg.bind = need(&mut it, "bind")?,
                "shards" => {
                    cfg.shards = need(&mut it, "shards")?
                        .parse()
                        .map_err(|_| "invalid shards")?
                }
                "maxmemory" => cfg.maxmemory = parse_mem(&need(&mut it, "maxmemory")?)?,
                "appendonly" => {
                    let v = need(&mut it, "appendonly")?;
                    cfg.appendonly =
                        matches!(v.to_ascii_lowercase().as_str(), "yes" | "true" | "1");
                }
                "requirepass" => cfg.requirepass = Some(need(&mut it, "requirepass")?),
                "masterauth" => cfg.masterauth = Some(need(&mut it, "masterauth")?),
                "tls-cert" | "tls-cert-file" => {
                    cfg.tls_cert = Some(PathBuf::from(need(&mut it, "tls-cert")?))
                }
                "tls-key" | "tls-key-file" => {
                    cfg.tls_key = Some(PathBuf::from(need(&mut it, "tls-key")?))
                }
                "replicaof" | "slaveof" => {
                    // Accept "host:port" or "host port".
                    let first = need(&mut it, "replicaof")?;
                    let (host, port) = if let Some((h, p)) = first.split_once(':') {
                        (h.to_string(), p.to_string())
                    } else {
                        (first, need(&mut it, "replicaof")?)
                    };
                    let port: u16 = port.parse().map_err(|_| "invalid replicaof port")?;
                    cfg.replicaof = Some((host, port));
                }
                "dir" => cfg.dir = PathBuf::from(need(&mut it, "dir")?),
                "aof-file" | "appendfilename" => cfg.aof_file = need(&mut it, "aof-file")?,
                "snapshot-file" | "dbfilename" => {
                    cfg.snapshot_file = need(&mut it, "snapshot-file")?
                }
                other => eprintln!("warning: ignoring unknown option --{other}"),
            }
        }
        Ok(cfg)
    }
}

fn need(it: &mut impl Iterator<Item = String>, name: &str) -> Result<String, String> {
    it.next()
        .ok_or_else(|| format!("--{name} requires a value"))
}

/// Parse a memory size like `512mb`, `1gb`, `1048576`.
fn parse_mem(s: &str) -> Result<usize, String> {
    let s = s.trim().to_ascii_lowercase();
    let (num, mult) = if let Some(p) = s.strip_suffix("gb") {
        (p, 1usize << 30)
    } else if let Some(p) = s.strip_suffix("mb") {
        (p, 1usize << 20)
    } else if let Some(p) = s.strip_suffix("kb") {
        (p, 1usize << 10)
    } else if let Some(p) = s.strip_suffix('b') {
        (p, 1)
    } else {
        (s.as_str(), 1)
    };
    num.trim()
        .parse::<usize>()
        .map(|n| n * mult)
        .map_err(|_| format!("invalid memory size: {s}"))
}

pub const HELP: &str = "\
inmemd — fast in-memory cache server (RESP2/3)

USAGE:
  inmemd [OPTIONS]

OPTIONS:
  --port <N>             listen port (default 6380)
  --bind <ADDR>          bind address (default 127.0.0.1)
  --shards <N>           number of store partitions for concurrency (default 512)
  --maxmemory <SIZE>     memory budget, e.g. 512mb, 2gb (default: unbounded)
  --appendonly <yes|no>  enable AOF persistence (default no)
  --requirepass <PASS>   require AUTH with this password (default none)
  --replicaof <H:P>      run as a read-only replica of primary host:port
  --masterauth <PASS>    password to authenticate to the primary
  --tls-cert <FILE>      PEM cert chain to enable TLS (needs `--features tls` build)
  --tls-key <FILE>       PEM private key for TLS
  --dir <PATH>           directory for persistence files (default .)
  --aof-file <NAME>      AOF filename (default inmem.aof)
  --snapshot-file <NAME> snapshot filename (default inmem.snapshot)
  --help                 show this help
";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_flags_and_mem() {
        let args = ["--port", "7000", "--maxmemory", "256mb", "--shards", "8"]
            .into_iter()
            .map(String::from);
        let c = Config::from_args(args).unwrap();
        assert_eq!(c.port, 7000);
        assert_eq!(c.maxmemory, 256 << 20);
        assert_eq!(c.shards, 8);
    }

    #[test]
    fn mem_units() {
        assert_eq!(parse_mem("1gb").unwrap(), 1 << 30);
        assert_eq!(parse_mem("1024").unwrap(), 1024);
        assert!(parse_mem("bogus").is_err());
    }
}
