//! inmem-ui — a web dashboard bridge for the inmem cache server.
//!
//! Browsers can't speak the RESP wire protocol, so this process sits between them and `inmemd`:
//! it holds a RESP/TCP connection to the server and exposes an HTTP + WebSocket API, plus the
//! static dashboard. The `inmemd` server itself is never modified.
//!
//! Usage:
//!   inmem-ui [--listen 127.0.0.1:8080] [--inmem 127.0.0.1:6380] [--requirepass PASS]

mod resp;

use std::sync::Arc;
use std::time::Instant;

use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Query, State};
use axum::http::{header, StatusCode};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{json, Value};
use tokio::sync::Mutex;

use resp::{reply_items, reply_str, RespClient};

/// Shared state: connection target plus one lazily-(re)connected RESP client for the REST API.
struct AppState {
    addr: String,
    pass: Option<String>,
    conn: Mutex<Option<RespClient>>,
}

impl AppState {
    /// Run a command, reconnecting once if the existing connection is dead.
    async fn exec(&self, args: &[&[u8]]) -> Result<Value, String> {
        let mut guard = self.conn.lock().await;
        for attempt in 0..2 {
            if guard.is_none() {
                match RespClient::connect(&self.addr, self.pass.as_deref()).await {
                    Ok(c) => *guard = Some(c),
                    Err(e) => return Err(format!("connect {}: {e}", self.addr)),
                }
            }
            let client = guard.as_mut().unwrap();
            match client.cmd(args).await {
                Ok(v) => return Ok(v),
                Err(e) => {
                    *guard = None; // drop the broken connection and retry once
                    if attempt == 1 {
                        return Err(format!("command failed: {e}"));
                    }
                }
            }
        }
        unreachable!()
    }
}

#[tokio::main]
async fn main() {
    let mut listen = "127.0.0.1:8080".to_string();
    let mut inmem = "127.0.0.1:6380".to_string();
    let mut pass: Option<String> = None;

    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        match a.as_str() {
            "--listen" => listen = args.next().unwrap_or(listen),
            "--inmem" => inmem = args.next().unwrap_or(inmem),
            "--requirepass" => pass = args.next(),
            "--help" | "-h" => {
                println!(
                    "inmem-ui — web dashboard for inmem\n\n\
                     OPTIONS:\n  \
                     --listen <ADDR:PORT>   web UI bind address (default 127.0.0.1:8080)\n  \
                     --inmem  <HOST:PORT>   inmemd address to proxy (default 127.0.0.1:6380)\n  \
                     --requirepass <PASS>   password if inmemd requires AUTH\n"
                );
                return;
            }
            other => eprintln!("warning: ignoring unknown option {other}"),
        }
    }

    let state = Arc::new(AppState {
        addr: inmem.clone(),
        pass,
        conn: Mutex::new(None),
    });

    let app = Router::new()
        .route("/", get(index))
        .route("/app.js", get(app_js))
        .route("/styles.css", get(styles_css))
        .route("/api/info", get(api_info))
        .route("/api/keys", get(api_keys))
        .route("/api/value", get(api_value))
        .route("/api/command", post(api_command))
        .route("/api/stream", get(api_stream))
        .with_state(state);

    let listener = match tokio::net::TcpListener::bind(&listen).await {
        Ok(l) => l,
        Err(e) => {
            eprintln!("failed to bind {listen}: {e}");
            std::process::exit(1);
        }
    };
    println!("inmem-ui listening on http://{listen}  ->  inmemd at {inmem}");
    axum::serve(listener, app).await.unwrap();
}

// ---------- static assets ----------

async fn index() -> Html<&'static str> {
    Html(include_str!("static/index.html"))
}
async fn app_js() -> impl IntoResponse {
    (
        [(
            header::CONTENT_TYPE,
            "application/javascript; charset=utf-8",
        )],
        include_str!("static/app.js"),
    )
}
async fn styles_css() -> impl IntoResponse {
    (
        [(header::CONTENT_TYPE, "text/css; charset=utf-8")],
        include_str!("static/styles.css"),
    )
}

// ---------- REST API ----------

fn err_json(msg: String) -> Response {
    (StatusCode::BAD_GATEWAY, Json(json!({ "error": msg }))).into_response()
}

/// Parse the flat `INFO` text into a `{ section: { key: value } }` map, plus a live PING latency.
async fn api_info(State(st): State<Arc<AppState>>) -> Response {
    let started = Instant::now();
    let ping = st.exec(&[b"PING"]).await;
    let ping_ms = started.elapsed().as_secs_f64() * 1000.0;
    if let Err(e) = ping {
        return err_json(e);
    }

    let info = match st.exec(&[b"INFO"]).await {
        Ok(v) => reply_str(&v).unwrap_or_default(),
        Err(e) => return err_json(e),
    };
    let dbsize = match st.exec(&[b"DBSIZE"]).await {
        Ok(v) => v.get("int").and_then(Value::as_i64).unwrap_or(0),
        Err(e) => return err_json(e),
    };

    let mut sections = serde_json::Map::new();
    let mut current = "General".to_string();
    for line in info.lines() {
        let line = line.trim();
        if let Some(name) = line.strip_prefix("# ") {
            current = name.to_string();
        } else if let Some((k, v)) = line.split_once(':') {
            sections
                .entry(current.clone())
                .or_insert_with(|| json!({}))
                .as_object_mut()
                .unwrap()
                .insert(k.to_string(), json!(v));
        }
    }

    Json(json!({
        "sections": sections,
        "dbsize": dbsize,
        "ping_ms": (ping_ms * 100.0).round() / 100.0,
    }))
    .into_response()
}

#[derive(Deserialize)]
struct KeysQuery {
    #[serde(default = "default_match")]
    r#match: String,
    #[serde(default = "default_limit")]
    limit: usize,
}
fn default_match() -> String {
    "*".into()
}
fn default_limit() -> usize {
    200
}

/// List keys via SCAN, enriched with TYPE and TTL for each (bounded by `limit`).
async fn api_keys(State(st): State<Arc<AppState>>, Query(q): Query<KeysQuery>) -> Response {
    let scan = match st
        .exec(&[
            b"SCAN",
            b"0",
            b"MATCH",
            q.r#match.as_bytes(),
            b"COUNT",
            b"1000",
        ])
        .await
    {
        Ok(v) => v,
        Err(e) => return err_json(e),
    };
    // SCAN reply: [ cursor, [ keys... ] ]
    let outer = reply_items(&scan);
    let names: Vec<String> = outer
        .get(1)
        .map(reply_items)
        .unwrap_or_default()
        .iter()
        .filter_map(reply_str)
        .collect();
    let total = names.len();

    let mut keys = Vec::new();
    for name in names.into_iter().take(q.limit) {
        let kbytes = name.as_bytes();
        let ktype = st
            .exec(&[b"TYPE", kbytes])
            .await
            .ok()
            .and_then(|v| reply_str(&v))
            .unwrap_or_else(|| "unknown".into());
        let ttl = st
            .exec(&[b"TTL", kbytes])
            .await
            .ok()
            .and_then(|v| v.get("int").and_then(Value::as_i64))
            .unwrap_or(-1);
        keys.push(json!({ "name": name, "type": ktype, "ttl": ttl }));
    }

    Json(json!({ "keys": keys, "total": total, "truncated": total > q.limit })).into_response()
}

#[derive(Deserialize)]
struct ValueQuery {
    key: String,
}

/// Fetch a key's value, shaped by its type.
async fn api_value(State(st): State<Arc<AppState>>, Query(q): Query<ValueQuery>) -> Response {
    let k = q.key.as_bytes();
    let ktype = match st.exec(&[b"TYPE", k]).await {
        Ok(v) => reply_str(&v).unwrap_or_else(|| "none".into()),
        Err(e) => return err_json(e),
    };

    let value: Value = match ktype.as_str() {
        "string" => st
            .exec(&[b"GET", k])
            .await
            .map(|v| json!(reply_str(&v)))
            .unwrap_or(Value::Null),
        "list" => list_value(&st, &[b"LRANGE", k, b"0", b"-1"]).await,
        "set" => list_value(&st, &[b"SMEMBERS", k]).await,
        "hash" => pairs_value(&st, &[b"HGETALL", k]).await,
        "zset" => pairs_value(&st, &[b"ZRANGE", k, b"0", b"-1", b"WITHSCORES"]).await,
        _ => Value::Null,
    };

    let ttl = st
        .exec(&[b"TTL", k])
        .await
        .ok()
        .and_then(|v| v.get("int").and_then(Value::as_i64))
        .unwrap_or(-1);

    Json(json!({ "key": q.key, "type": ktype, "ttl": ttl, "value": value })).into_response()
}

async fn list_value(st: &Arc<AppState>, args: &[&[u8]]) -> Value {
    match st.exec(args).await {
        Ok(v) => json!(reply_items(&v)
            .iter()
            .filter_map(reply_str)
            .collect::<Vec<_>>()),
        Err(_) => Value::Null,
    }
}

async fn pairs_value(st: &Arc<AppState>, args: &[&[u8]]) -> Value {
    match st.exec(args).await {
        Ok(v) => {
            let flat: Vec<String> = reply_items(&v).iter().filter_map(reply_str).collect();
            let pairs: Vec<Value> = flat
                .chunks(2)
                .map(|c| json!({ "field": c[0], "value": c.get(1).cloned().unwrap_or_default() }))
                .collect();
            json!(pairs)
        }
        Err(_) => Value::Null,
    }
}

#[derive(Deserialize)]
struct CommandBody {
    args: Vec<String>,
}

/// Run an arbitrary command (the console). Local admin tool — no command filtering.
async fn api_command(State(st): State<Arc<AppState>>, Json(body): Json<CommandBody>) -> Response {
    if body.args.is_empty() {
        return err_json("empty command".into());
    }
    let byte_args: Vec<&[u8]> = body.args.iter().map(|s| s.as_bytes()).collect();
    match st.exec(&byte_args).await {
        Ok(v) => Json(json!({ "reply": v })).into_response(),
        Err(e) => err_json(e),
    }
}

// ---------- WebSocket metrics stream ----------

async fn api_stream(ws: WebSocketUpgrade, State(st): State<Arc<AppState>>) -> Response {
    ws.on_upgrade(move |socket| stream_metrics(socket, st))
}

/// Extract a numeric `key:value` field from the flat `INFO` text.
fn info_field(text: &str, key: &str) -> Option<u64> {
    text.lines().find_map(|line| {
        line.trim()
            .strip_prefix(key)
            .and_then(|r| r.strip_prefix(':'))
            .and_then(|v| v.trim().parse().ok())
    })
}

/// Push a metrics snapshot roughly once a second. Rates (ops/sec, hit-ratio) are computed from the
/// delta between successive `INFO` reads, so they reflect real server-wide activity, not just UI
/// traffic; PING latency is measured directly by the bridge.
async fn stream_metrics(mut socket: WebSocket, st: Arc<AppState>) {
    let mut tick = tokio::time::interval(std::time::Duration::from_secs(1));
    // Previous sample: (timestamp_ms, total_commands, hits, misses).
    let mut prev: Option<(u128, u64, u64, u64)> = None;

    loop {
        tick.tick().await;

        let started = Instant::now();
        let ping_ok = st.exec(&[b"PING"]).await.is_ok();
        let ping_ms = started.elapsed().as_secs_f64() * 1000.0;

        let info = st
            .exec(&[b"INFO"])
            .await
            .ok()
            .and_then(|v| reply_str(&v))
            .unwrap_or_default();
        let dbsize = st
            .exec(&[b"DBSIZE"])
            .await
            .ok()
            .and_then(|v| v.get("int").and_then(Value::as_i64))
            .unwrap_or(0);

        let commands = info_field(&info, "total_commands_processed").unwrap_or(0);
        let hits = info_field(&info, "keyspace_hits").unwrap_or(0);
        let misses = info_field(&info, "keyspace_misses").unwrap_or(0);
        let evicted = info_field(&info, "evicted_keys").unwrap_or(0);
        let used_memory = info_field(&info, "used_memory").unwrap_or(0);
        let maxmemory = info_field(&info, "maxmemory").unwrap_or(0);
        let now = now_ms();

        // ops/sec and window hit-ratio come from the change since the previous tick.
        let (ops_per_sec, hit_ratio) = match prev {
            Some((pts, pc, ph, pm)) => {
                let dt = (now.saturating_sub(pts)) as f64 / 1000.0;
                let ops = if dt > 0.0 {
                    commands.saturating_sub(pc) as f64 / dt
                } else {
                    0.0
                };
                let (dh, dm) = (hits.saturating_sub(ph), misses.saturating_sub(pm));
                let hr = if dh + dm > 0 {
                    dh as f64 / (dh + dm) as f64 * 100.0
                } else if hits + misses > 0 {
                    // No reads this window — fall back to the cumulative ratio.
                    hits as f64 / (hits + misses) as f64 * 100.0
                } else {
                    0.0
                };
                (ops, hr)
            }
            None => (0.0, 0.0),
        };
        prev = Some((now, commands, hits, misses));

        let msg = json!({
            "ts": now,
            "online": ping_ok,
            "ping_ms": (ping_ms * 100.0).round() / 100.0,
            "dbsize": dbsize,
            "ops_per_sec": ops_per_sec.round(),
            "hit_ratio": (hit_ratio * 10.0).round() / 10.0,
            "total_commands": commands,
            "hits": hits,
            "misses": misses,
            "evicted": evicted,
            "used_memory": used_memory,
            "maxmemory": maxmemory,
        });
        if socket.send(Message::Text(msg.to_string())).await.is_err() {
            break; // browser disconnected
        }
    }
}

fn now_ms() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0)
}
