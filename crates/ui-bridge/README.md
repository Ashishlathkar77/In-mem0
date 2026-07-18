# inmem-ui — web dashboard for inmem

A RedisInsight-style admin console for the [`inmem`](../../README.md) cache server: live
metrics, a key browser with a type-aware value inspector, and a `redis-cli`-style command
console — all in the browser.

## Why a bridge?

Browsers can't open raw TCP sockets or speak the RESP wire protocol, so a web UI can't talk to
`inmemd` directly. `inmem-ui` sits in between:

```
Browser (dashboard)  ──HTTP + WebSocket──►  inmem-ui bridge  ──RESP/TCP──►  inmemd
```

The bridge holds a RESP client connection to the server, exposes a small JSON API plus a
WebSocket metrics stream, and serves the static dashboard. **The `inmemd` server is never
modified** — the whole UI lives in this crate.

## Run it

```bash
# 1. start inmemd (from the repo root)
cargo run --release -p inmem-server -- --port 6380

# 2. start the dashboard bridge
cargo run -p inmem-ui-bridge         # or the release build for lower overhead

# 3. open the console
open http://127.0.0.1:8080
```

Options:

| Flag | Default | Meaning |
|------|---------|---------|
| `--listen <ADDR:PORT>` | `127.0.0.1:8080` | where the web UI is served |
| `--inmem <HOST:PORT>`  | `127.0.0.1:6380` | the `inmemd` instance to proxy |
| `--requirepass <PASS>` | none | password if `inmemd` was started with `--requirepass` |

## What the dashboard shows

A developer-tool UI (per the "inmem console redesign" Claude Design spec): a fixed left sidebar
(logo, ⌘K search, Cluster nav, a **dark/light theme toggle** persisted to `localStorage`, and live
connection status), IBM Plex Sans + JetBrains Mono, and four screens. A **⌘K command palette**
(also the sidebar Search button) jumps between screens or drops a command into the console. The
default dark theme and the light "Daylight" theme share one layout, driven entirely by CSS
variables so charts and badges recolor with the toggle.

- **Overview** — four metric tiles (keys, throughput, hit ratio, memory used/max) with gradient
  sparklines for throughput, hit ratio, and latency, fed by a 1 Hz WebSocket stream, plus the
  full parsed `INFO` grid.
- **Keys** — `SCAN`-based browser with a `MATCH` box and per-type filter chips; each key shows a
  colored type badge and live TTL. Selecting a key opens an inspector that renders its value per
  type (string / list / set / hash / zset) with Copy and Delete actions.
- **Console** — an interactive REPL with a live metric strip and quick-command chips. Commands run
  through the bridge; replies are formatted `redis-cli`-style (multi-line `INFO`, numbered
  arrays). `↑`/`↓` walk command history.
- **Monitor** — a live view: a large throughput panel with peak / hit-ratio / ping, plus
  keyspace-and-memory and S3-FIFO eviction stat panels. The per-command mix / slow log the design
  sketches aren't tracked by the server, so that panel says so rather than showing fake numbers.

> The static assets (`src/static/*`) are embedded into the binary with `include_str!`, so a change
> to the HTML/CSS/JS needs a `cargo build -p inmem-ui-bridge` to take effect.

## HTTP API

The frontend is just a client of these endpoints — useful on their own for scripting:

| Method | Path | Purpose |
|--------|------|---------|
| `GET`  | `/api/info` | parsed `INFO` sections + `DBSIZE` + PING latency |
| `GET`  | `/api/keys?match=<glob>&limit=<n>` | keys with type + TTL |
| `GET`  | `/api/value?key=<name>` | a key's value, shaped by type |
| `POST` | `/api/command` | run an arbitrary command; body `{"args": ["GET","k"]}` |
| `GET`  | `/api/stream` | WebSocket; each second pushes `{online, ping_ms, dbsize, ops_per_sec, hit_ratio, total_commands, hits, misses, evicted, used_memory, maxmemory}` |

## Notes / limits

- Throughput (ops/sec) and hit-ratio are computed by the bridge from the delta between successive
  `INFO` reads of `total_commands_processed` / `keyspace_hits` / `keyspace_misses` — real
  server-wide activity (including the bridge's own 1 Hz probes), not just UI traffic. Those
  counters live in the store as per-shard tallies (mutated under the shard lock, so no added
  contention) plus one relaxed atomic for the command count; see
  [`Store::stats`](../core/src/store.rs) and [`info_text`](../server/src/commands.rs). PING
  latency is measured directly by the bridge.
- `SCAN` currently returns all matching keys in one pass (the server ignores the cursor), so the
  key browser bounds itself with `limit` and flags when the result was truncated.
- The command console applies no command filtering — it's a local admin tool. Don't expose the
  bridge on an untrusted network.
