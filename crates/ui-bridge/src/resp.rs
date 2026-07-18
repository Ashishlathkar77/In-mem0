//! A minimal async RESP2 *client* for talking to an inmem/Redis server.
//!
//! The `inmem-proto` crate parses commands and encodes replies (the server's side of the
//! conversation). The bridge is a client, so it needs the mirror image: encode a command and
//! parse a reply. That reply is decoded into a `serde_json::Value` so HTTP handlers can hand it
//! straight to the browser.

use std::future::Future;
use std::io;
use std::pin::Pin;

use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::tcp::{OwnedReadHalf, OwnedWriteHalf};
use tokio::net::TcpStream;

/// A single RESP connection. Not shared directly — wrap in a `Mutex` (see `AppState`).
pub struct RespClient {
    r: BufReader<OwnedReadHalf>,
    w: OwnedWriteHalf,
}

impl RespClient {
    /// Open a connection and, if `pass` is set, authenticate.
    pub async fn connect(addr: &str, pass: Option<&str>) -> io::Result<Self> {
        let stream = TcpStream::connect(addr).await?;
        stream.set_nodelay(true).ok();
        let (r, w) = stream.into_split();
        let mut c = RespClient {
            r: BufReader::new(r),
            w,
        };
        if let Some(p) = pass {
            let reply = c.cmd(&[b"AUTH", p.as_bytes()]).await?;
            if let Some("error") = reply.get("type").and_then(Value::as_str) {
                let msg = reply
                    .get("str")
                    .and_then(Value::as_str)
                    .unwrap_or("AUTH failed");
                return Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    msg.to_string(),
                ));
            }
        }
        Ok(c)
    }

    /// Send one command (array of bulk args) and read exactly one reply.
    pub async fn cmd(&mut self, args: &[&[u8]]) -> io::Result<Value> {
        let mut out = Vec::new();
        encode_cmd(args, &mut out);
        self.w.write_all(&out).await?;
        self.w.flush().await?;
        read_reply(&mut self.r).await
    }
}

/// Encode a command as a RESP array of bulk strings.
fn encode_cmd(args: &[&[u8]], out: &mut Vec<u8>) {
    out.extend_from_slice(format!("*{}\r\n", args.len()).as_bytes());
    for a in args {
        out.extend_from_slice(format!("${}\r\n", a.len()).as_bytes());
        out.extend_from_slice(a);
        out.extend_from_slice(b"\r\n");
    }
}

/// Read one RESP reply, decoding into a tagged JSON value:
/// `{type:"status"|"error"|"bulk", str}`, `{type:"int", int}`, `{type:"array", items}`, `{type:"nil"}`.
fn read_reply<'a>(
    r: &'a mut BufReader<OwnedReadHalf>,
) -> Pin<Box<dyn Future<Output = io::Result<Value>> + Send + 'a>> {
    Box::pin(async move {
        let mut line = String::new();
        let n = r.read_line(&mut line).await?;
        if n == 0 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "connection closed",
            ));
        }
        let line = line.trim_end_matches(['\r', '\n']);
        let (tag, rest) = line.split_at(1);
        match tag {
            "+" => Ok(json!({ "type": "status", "str": rest })),
            "-" => Ok(json!({ "type": "error", "str": rest })),
            ":" => Ok(json!({ "type": "int", "int": rest.parse::<i64>().unwrap_or(0) })),
            "$" => {
                let len: i64 = rest.parse().unwrap_or(-1);
                if len < 0 {
                    return Ok(json!({ "type": "nil" }));
                }
                let mut buf = vec![0u8; len as usize + 2]; // data + trailing CRLF
                r.read_exact(&mut buf).await?;
                buf.truncate(len as usize);
                Ok(json!({ "type": "bulk", "str": String::from_utf8_lossy(&buf) }))
            }
            "*" | "~" | ">" => {
                let count: i64 = rest.parse().unwrap_or(-1);
                if count < 0 {
                    return Ok(json!({ "type": "nil" }));
                }
                let mut items = Vec::with_capacity(count as usize);
                for _ in 0..count {
                    items.push(read_reply(r).await?);
                }
                Ok(json!({ "type": "array", "items": items }))
            }
            "%" => {
                // RESP3 map: N key/value pairs. Flatten to an array of 2N items.
                let pairs: i64 = rest.parse().unwrap_or(0);
                let mut items = Vec::with_capacity(pairs as usize * 2);
                for _ in 0..pairs * 2 {
                    items.push(read_reply(r).await?);
                }
                Ok(json!({ "type": "array", "items": items }))
            }
            _ => Ok(json!({ "type": "status", "str": line })),
        }
    })
}

/// Pull a UTF-8 string out of a `bulk` or `status` reply.
pub fn reply_str(v: &Value) -> Option<String> {
    match v.get("type").and_then(Value::as_str)? {
        "bulk" | "status" | "error" => v.get("str").and_then(Value::as_str).map(String::from),
        "int" => v.get("int").and_then(Value::as_i64).map(|n| n.to_string()),
        _ => None,
    }
}

/// Pull the item list out of an `array` reply.
pub fn reply_items(v: &Value) -> Vec<Value> {
    v.get("items")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default()
}
