//! RESP2/RESP3 parsing and encoding.

use std::fmt;

/// A protocol-level error (malformed input). The server turns this into a connection close.
#[derive(Debug, PartialEq, Eq)]
pub enum ProtocolError {
    /// A frame was structurally invalid (e.g. bad length prefix, missing CRLF where required).
    Malformed(&'static str),
    /// Declared length exceeds the configured safety bound.
    TooLarge,
}

impl fmt::Display for ProtocolError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ProtocolError::Malformed(m) => write!(f, "protocol error: {m}"),
            ProtocolError::TooLarge => write!(f, "protocol error: frame too large"),
        }
    }
}
impl std::error::Error for ProtocolError {}

/// A parsed command (argument vector) plus the number of input bytes it consumed.
pub type Command = (Vec<Vec<u8>>, usize);

/// Hard cap on a single bulk string / array length to bound memory from a hostile client.
const MAX_BULK_LEN: i64 = 512 * 1024 * 1024; // 512 MiB, same order as Redis proto-max-bulk-len
const MAX_ARRAY_LEN: i64 = 1024 * 1024;

/// Find the index just past the next CRLF starting at `start`, returning the line slice
/// (excluding CRLF) and the index after the CRLF. Returns None if no CRLF yet.
fn read_line(buf: &[u8], start: usize) -> Option<(&[u8], usize)> {
    let mut i = start;
    while i + 1 < buf.len() {
        if buf[i] == b'\r' && buf[i + 1] == b'\n' {
            return Some((&buf[start..i], i + 2));
        }
        i += 1;
    }
    None
}

fn parse_int(line: &[u8]) -> Result<i64, ProtocolError> {
    std::str::from_utf8(line)
        .ok()
        .and_then(|s| s.parse::<i64>().ok())
        .ok_or(ProtocolError::Malformed("invalid integer"))
}

/// Parse one client command from `buf`.
///
/// Returns:
/// - `Ok(Some((argv, consumed)))` — a full command; `consumed` bytes may be dropped from `buf`.
/// - `Ok(None)` — need more bytes (incomplete frame).
/// - `Err(_)` — malformed; the server should close the connection.
///
/// Accepts the standard RESP array-of-bulk-strings form (`*N\r\n$len\r\n...`) and the legacy
/// inline form (a plain `PING\r\n` line), which `redis-cli` and ad-hoc tools sometimes use.
pub fn parse_command(buf: &[u8]) -> Result<Option<Command>, ProtocolError> {
    if buf.is_empty() {
        return Ok(None);
    }
    if buf[0] == b'*' {
        parse_array_command(buf)
    } else {
        parse_inline_command(buf)
    }
}

fn parse_array_command(buf: &[u8]) -> Result<Option<Command>, ProtocolError> {
    let Some((line, mut pos)) = read_line(buf, 1) else {
        return Ok(None);
    };
    let n = parse_int(line)?;
    if n < 0 {
        // Null/empty array as a command: treat as an empty command (skip it).
        return Ok(Some((Vec::new(), pos)));
    }
    if n > MAX_ARRAY_LEN {
        return Err(ProtocolError::TooLarge);
    }
    let mut argv = Vec::with_capacity(n as usize);
    for _ in 0..n {
        if pos >= buf.len() {
            return Ok(None);
        }
        if buf[pos] != b'$' {
            return Err(ProtocolError::Malformed("expected bulk string in command"));
        }
        let Some((len_line, after_len)) = read_line(buf, pos + 1) else {
            return Ok(None);
        };
        let len = parse_int(len_line)?;
        if len < 0 {
            return Err(ProtocolError::Malformed("null bulk in command"));
        }
        if len > MAX_BULK_LEN {
            return Err(ProtocolError::TooLarge);
        }
        let len = len as usize;
        let data_end = after_len + len;
        // need data + trailing CRLF
        if data_end + 2 > buf.len() {
            return Ok(None);
        }
        if &buf[data_end..data_end + 2] != b"\r\n" {
            return Err(ProtocolError::Malformed("bulk not CRLF-terminated"));
        }
        argv.push(buf[after_len..data_end].to_vec());
        pos = data_end + 2;
    }
    Ok(Some((argv, pos)))
}

fn parse_inline_command(buf: &[u8]) -> Result<Option<Command>, ProtocolError> {
    let Some((line, consumed)) = read_line(buf, 0) else {
        // Guard against an unbounded inline line with no CRLF.
        if buf.len() > 64 * 1024 {
            return Err(ProtocolError::TooLarge);
        }
        return Ok(None);
    };
    let argv = line
        .split(|b| b.is_ascii_whitespace())
        .filter(|s| !s.is_empty())
        .map(|s| s.to_vec())
        .collect();
    Ok(Some((argv, consumed)))
}

/// Zero-copy command parse: instead of allocating an owned argument vector, fill `argv` with
/// `(offset, len)` ranges into `buf`. This lets the hot path read arguments as borrowed slices
/// (`&buf[offset..offset+len]`) with no per-argument allocation or copy.
///
/// `argv` is cleared first. Returns `Ok(Some(consumed))`, `Ok(None)` (incomplete), or `Err`.
pub fn parse_command_ranges(
    buf: &[u8],
    argv: &mut Vec<(usize, usize)>,
) -> Result<Option<usize>, ProtocolError> {
    argv.clear();
    if buf.is_empty() {
        return Ok(None);
    }
    if buf[0] == b'*' {
        let Some((line, mut pos)) = read_line(buf, 1) else {
            return Ok(None);
        };
        let n = parse_int(line)?;
        if n < 0 {
            return Ok(Some(pos));
        }
        if n > MAX_ARRAY_LEN {
            return Err(ProtocolError::TooLarge);
        }
        argv.reserve(n as usize);
        for _ in 0..n {
            if pos >= buf.len() {
                return Ok(None);
            }
            if buf[pos] != b'$' {
                return Err(ProtocolError::Malformed("expected bulk string in command"));
            }
            let Some((len_line, after_len)) = read_line(buf, pos + 1) else {
                return Ok(None);
            };
            let len = parse_int(len_line)?;
            if len < 0 {
                return Err(ProtocolError::Malformed("null bulk in command"));
            }
            if len > MAX_BULK_LEN {
                return Err(ProtocolError::TooLarge);
            }
            let len = len as usize;
            let data_end = after_len + len;
            if data_end + 2 > buf.len() {
                return Ok(None);
            }
            if &buf[data_end..data_end + 2] != b"\r\n" {
                return Err(ProtocolError::Malformed("bulk not CRLF-terminated"));
            }
            argv.push((after_len, len));
            pos = data_end + 2;
        }
        Ok(Some(pos))
    } else {
        let Some((line, consumed)) = read_line(buf, 0) else {
            if buf.len() > 64 * 1024 {
                return Err(ProtocolError::TooLarge);
            }
            return Ok(None);
        };
        // Inline form: record offsets of whitespace-delimited tokens.
        let mut i = 0;
        while i < line.len() {
            while i < line.len() && line[i].is_ascii_whitespace() {
                i += 1;
            }
            let start = i;
            while i < line.len() && !line[i].is_ascii_whitespace() {
                i += 1;
            }
            if i > start {
                argv.push((start, i - start));
            }
        }
        Ok(Some(consumed))
    }
}

/// Write a bulk string reply directly into `out` (no intermediate allocation).
pub fn write_bulk(out: &mut Vec<u8>, data: &[u8]) {
    out.push(b'$');
    write_uint(out, data.len() as u64);
    out.extend_from_slice(b"\r\n");
    out.extend_from_slice(data);
    out.extend_from_slice(b"\r\n");
}

/// Write a null (RESP3 `_` / RESP2 null bulk) directly into `out`.
pub fn write_null(out: &mut Vec<u8>, resp3: bool) {
    out.extend_from_slice(if resp3 { b"_\r\n" } else { b"$-1\r\n" });
}

/// Write a simple-string reply (`+s\r\n`) directly into `out`.
pub fn write_simple(out: &mut Vec<u8>, s: &str) {
    out.push(b'+');
    out.extend_from_slice(s.as_bytes());
    out.extend_from_slice(b"\r\n");
}

/// Write an error reply (`-msg\r\n`) directly into `out`.
pub fn write_error(out: &mut Vec<u8>, msg: &str) {
    out.push(b'-');
    out.extend_from_slice(msg.as_bytes());
    out.extend_from_slice(b"\r\n");
}

/// Write an integer reply (`:n\r\n`) directly into `out`.
pub fn write_int(out: &mut Vec<u8>, n: i64) {
    out.push(b':');
    if n < 0 {
        out.push(b'-');
        write_uint(out, n.unsigned_abs());
    } else {
        write_uint(out, n as u64);
    }
    out.extend_from_slice(b"\r\n");
}

/// Append a u64 in decimal without allocating (writes into a stack buffer first).
fn write_uint(out: &mut Vec<u8>, mut v: u64) {
    if v == 0 {
        out.push(b'0');
        return;
    }
    let mut tmp = [0u8; 20];
    let mut i = tmp.len();
    while v > 0 {
        i -= 1;
        tmp[i] = b'0' + (v % 10) as u8;
        v /= 10;
    }
    out.extend_from_slice(&tmp[i..]);
}

/// A server reply. Encodes to RESP2 by default; RESP3 differs only for nulls, booleans, doubles,
/// maps, and push frames (selected via `resp3` in [`Reply::encode`]).
#[derive(Debug, Clone, PartialEq)]
pub enum Reply {
    /// `+OK\r\n`
    Simple(String),
    /// `-ERR ...\r\n`
    Error(String),
    /// `:N\r\n`
    Int(i64),
    /// Bulk string, or null bulk when `None`.
    Bulk(Option<Vec<u8>>),
    /// Array, or null array when `None`.
    Array(Option<Vec<Reply>>),
    /// Null (RESP3 `_\r\n`; RESP2 falls back to null bulk `$-1`).
    Null,
    /// RESP3 boolean (`#t`/`#f`); RESP2 falls back to `:1`/`:0`.
    Bool(bool),
    /// RESP3 double (`,3.14`); RESP2 falls back to a bulk string.
    Double(f64),
    /// RESP3 map (`%N`); RESP2 falls back to a flat array.
    Map(Vec<(Reply, Reply)>),
}

impl Reply {
    /// Convenience: an `+OK` reply.
    pub fn ok() -> Reply {
        Reply::Simple("OK".into())
    }

    /// Serialize into `out`, choosing RESP2 or RESP3 encodings.
    pub fn encode(&self, out: &mut Vec<u8>, resp3: bool) {
        match self {
            Reply::Simple(s) => {
                out.push(b'+');
                out.extend_from_slice(s.as_bytes());
                out.extend_from_slice(b"\r\n");
            }
            Reply::Error(s) => {
                out.push(b'-');
                out.extend_from_slice(s.as_bytes());
                out.extend_from_slice(b"\r\n");
            }
            Reply::Int(n) => {
                out.push(b':');
                out.extend_from_slice(n.to_string().as_bytes());
                out.extend_from_slice(b"\r\n");
            }
            Reply::Bulk(None) => {
                out.extend_from_slice(if resp3 { b"_\r\n" } else { b"$-1\r\n" });
            }
            Reply::Bulk(Some(b)) => {
                out.push(b'$');
                out.extend_from_slice(b.len().to_string().as_bytes());
                out.extend_from_slice(b"\r\n");
                out.extend_from_slice(b);
                out.extend_from_slice(b"\r\n");
            }
            Reply::Array(None) => {
                out.extend_from_slice(if resp3 { b"_\r\n" } else { b"*-1\r\n" });
            }
            Reply::Array(Some(items)) => {
                out.push(b'*');
                out.extend_from_slice(items.len().to_string().as_bytes());
                out.extend_from_slice(b"\r\n");
                for it in items {
                    it.encode(out, resp3);
                }
            }
            Reply::Null => {
                out.extend_from_slice(if resp3 { b"_\r\n" } else { b"$-1\r\n" });
            }
            Reply::Bool(b) => {
                if resp3 {
                    out.extend_from_slice(if *b { b"#t\r\n" } else { b"#f\r\n" });
                } else {
                    out.extend_from_slice(if *b { b":1\r\n" } else { b":0\r\n" });
                }
            }
            Reply::Double(d) => {
                if resp3 {
                    out.push(b',');
                    out.extend_from_slice(format_double(*d).as_bytes());
                    out.extend_from_slice(b"\r\n");
                } else {
                    let s = format_double(*d);
                    Reply::Bulk(Some(s.into_bytes())).encode(out, resp3);
                }
            }
            Reply::Map(pairs) => {
                if resp3 {
                    out.push(b'%');
                    out.extend_from_slice(pairs.len().to_string().as_bytes());
                    out.extend_from_slice(b"\r\n");
                } else {
                    out.push(b'*');
                    out.extend_from_slice((pairs.len() * 2).to_string().as_bytes());
                    out.extend_from_slice(b"\r\n");
                }
                for (k, v) in pairs {
                    k.encode(out, resp3);
                    v.encode(out, resp3);
                }
            }
        }
    }

    /// Encode to a fresh `Vec` (convenience for tests).
    pub fn to_bytes(&self, resp3: bool) -> Vec<u8> {
        let mut v = Vec::new();
        self.encode(&mut v, resp3);
        v
    }
}

fn format_double(d: f64) -> String {
    if d.is_infinite() {
        if d > 0.0 {
            "inf".into()
        } else {
            "-inf".into()
        }
    } else if d.is_nan() {
        "nan".into()
    } else {
        // Trim trailing zeros for a clean representation.
        let s = format!("{d}");
        s
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_array_command_ok() {
        let buf = b"*2\r\n$3\r\nGET\r\n$3\r\nfoo\r\n";
        let (argv, consumed) = parse_command(buf).unwrap().unwrap();
        assert_eq!(argv, vec![b"GET".to_vec(), b"foo".to_vec()]);
        assert_eq!(consumed, buf.len());
    }

    #[test]
    fn parse_incomplete_returns_none() {
        // missing the value bytes + crlf
        let buf = b"*2\r\n$3\r\nGET\r\n$3\r\nfo";
        assert_eq!(parse_command(buf).unwrap(), None);
    }

    #[test]
    fn parse_two_pipelined_commands() {
        let buf = b"*1\r\n$4\r\nPING\r\n*1\r\n$4\r\nPING\r\n";
        let (a1, c1) = parse_command(buf).unwrap().unwrap();
        assert_eq!(a1, vec![b"PING".to_vec()]);
        let (a2, c2) = parse_command(&buf[c1..]).unwrap().unwrap();
        assert_eq!(a2, vec![b"PING".to_vec()]);
        assert_eq!(c1 + c2, buf.len());
    }

    #[test]
    fn parse_inline() {
        let buf = b"PING\r\n";
        let (argv, consumed) = parse_command(buf).unwrap().unwrap();
        assert_eq!(argv, vec![b"PING".to_vec()]);
        assert_eq!(consumed, buf.len());

        let buf2 = b"set foo bar\r\n";
        let (argv2, _) = parse_command(buf2).unwrap().unwrap();
        assert_eq!(
            argv2,
            vec![b"set".to_vec(), b"foo".to_vec(), b"bar".to_vec()]
        );
    }

    #[test]
    fn malformed_is_error() {
        let buf = b"*1\r\n+nope\r\n";
        assert!(parse_command(buf).is_err());
    }

    #[test]
    fn encode_resp2_vs_resp3_nulls() {
        assert_eq!(Reply::Null.to_bytes(false), b"$-1\r\n");
        assert_eq!(Reply::Null.to_bytes(true), b"_\r\n");
        assert_eq!(Reply::Bool(true).to_bytes(false), b":1\r\n");
        assert_eq!(Reply::Bool(true).to_bytes(true), b"#t\r\n");
    }

    #[test]
    fn ranges_parse_matches_owned() {
        let buf = b"*3\r\n$3\r\nSET\r\n$3\r\nfoo\r\n$3\r\nbar\r\n";
        let mut ranges = Vec::new();
        let consumed = parse_command_ranges(buf, &mut ranges).unwrap().unwrap();
        assert_eq!(consumed, buf.len());
        let args: Vec<&[u8]> = ranges.iter().map(|&(o, l)| &buf[o..o + l]).collect();
        assert_eq!(args, vec![&b"SET"[..], &b"foo"[..], &b"bar"[..]]);
    }

    #[test]
    fn ranges_parse_inline_and_incomplete() {
        let mut ranges = Vec::new();
        assert!(
            parse_command_ranges(b"*2\r\n$3\r\nGET\r\n$3\r\nfo", &mut ranges)
                .unwrap()
                .is_none()
        );
        let buf = b"set k v\r\n";
        let consumed = parse_command_ranges(buf, &mut ranges).unwrap().unwrap();
        assert_eq!(consumed, buf.len());
        let args: Vec<&[u8]> = ranges.iter().map(|&(o, l)| &buf[o..o + l]).collect();
        assert_eq!(args, vec![&b"set"[..], &b"k"[..], &b"v"[..]]);
    }

    #[test]
    fn direct_writers() {
        let mut o = Vec::new();
        write_bulk(&mut o, b"hi");
        assert_eq!(o, b"$2\r\nhi\r\n");
        o.clear();
        write_int(&mut o, -42);
        assert_eq!(o, b":-42\r\n");
        o.clear();
        write_null(&mut o, true);
        assert_eq!(o, b"_\r\n");
    }

    #[test]
    fn encode_basic_shapes() {
        assert_eq!(Reply::ok().to_bytes(false), b"+OK\r\n");
        assert_eq!(Reply::Int(42).to_bytes(false), b":42\r\n");
        assert_eq!(
            Reply::Bulk(Some(b"hi".to_vec())).to_bytes(false),
            b"$2\r\nhi\r\n"
        );
        let arr = Reply::Array(Some(vec![Reply::Int(1), Reply::Bulk(Some(b"x".to_vec()))]));
        assert_eq!(arr.to_bytes(false), b"*2\r\n:1\r\n$1\r\nx\r\n");
    }
}
