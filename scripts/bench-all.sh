#!/usr/bin/env bash
# Multi-framework cache benchmark: inmem vs Redis, Valkey, KeyDB, Memcached.
#
# Uses memtier_benchmark as the *uniform* driver so every system is measured the same way and
# Memcached (non-RESP) is included on equal footing. Reports aggregate ops/sec at several
# pipeline depths.
#
# Usage: scripts/bench-all.sh [requests_per_client] [clients] [threads]
# Requires: memtier_benchmark. Auto-detects each cache; missing ones are skipped.
# Dragonfly and Garnet are Linux/.NET-only — see scripts/bench-docker.md to include them.
set -uo pipefail

REQ="${1:-200000}"        # requests per client
CLIENTS="${2:-25}"        # connections per thread
THREADS="${3:-4}"         # memtier client threads  (=> CLIENTS*THREADS connections)
SHARDS="$(getconf _NPROCESSORS_ONLN 2>/dev/null || echo 8)"
PIPES=(1 16 64)
VALSIZE=64

cd "$(dirname "$0")/.."

# ---- locate binaries (PATH first, then Homebrew kegs) ----
keg() { echo "/opt/homebrew/opt/$1/bin/$2"; }
find_bin() { command -v "$1" 2>/dev/null || { [ -x "$2" ] && echo "$2"; }; }

INMEM="./target/release/inmemd"
REDIS="$(find_bin redis-server "")"
VALKEY="$(find_bin valkey-server "$(keg valkey valkey-server)")"
KEYDB="$(find_bin keydb-server "$(keg keydb keydb-server)")"
MEMCACHED="$(find_bin memcached "$(keg memcached memcached)")"
MEMTIER="$(find_bin memtier_benchmark "$(keg memtier_benchmark memtier_benchmark)")"

if [ -z "$MEMTIER" ]; then
  echo "memtier_benchmark not found (brew install memtier_benchmark). Aborting." >&2
  exit 1
fi

cargo build --release >/dev/null 2>&1 || { echo "build failed"; exit 1; }

PIDS=()
cleanup() { for p in "${PIDS[@]:-}"; do kill "$p" 2>/dev/null; done; }
trap cleanup EXIT

# run one memtier pass against proto/port, echo aggregate ops/sec for a given pipeline
run_one() {
  local proto="$1" port="$2" pipe="$3"
  "$MEMTIER" -s 127.0.0.1 -p "$port" -P "$proto" \
    -t "$THREADS" -c "$CLIENTS" -n "$REQ" --pipeline "$pipe" \
    --ratio 1:1 --data-size "$VALSIZE" --key-maximum 1000000 --hide-histogram 2>/dev/null \
    | awk '/^Totals/ {print $2}'
}

# Results go to a temp file (macOS ships bash 3.2, which has no associative arrays):
# each line is "name pipe ops".
RESFILE="$(mktemp)"
NAMES=()

bench_server() {
  local name="$1" proto="$2" port="$3"; shift 3
  local launch=("$@")
  "${launch[@]}" >"/tmp/bench-$name.log" 2>&1 &
  local pid=$!
  PIDS+=("$pid")
  sleep 1.2
  if ! kill -0 "$pid" 2>/dev/null; then
    echo "  ! $name failed to start (see /tmp/bench-$name.log)"; return
  fi
  NAMES+=("$name")
  local p ops
  for p in "${PIPES[@]}"; do
    printf "  %-10s -P %-3s ... " "$name" "$p"
    ops="$(run_one "$proto" "$port" "$p")"
    echo "$name $p ${ops:-NA}" >> "$RESFILE"
    printf "%s ops/sec\n" "${ops:-NA}"
  done
  kill "$pid" 2>/dev/null; wait "$pid" 2>/dev/null
}

lookup() { awk -v n="$1" -v p="$2" '$1==n && $2==p {print $3}' "$RESFILE"; }

echo "== config: $REQ req/client, $((CLIENTS*THREADS)) connections, value ${VALSIZE}B, pipelines ${PIPES[*]} =="
echo

bench_server inmem redis 7101 "$INMEM" --port 7101 --shards "$SHARDS"
[ -n "$REDIS" ]     && bench_server redis     redis    7102 "$REDIS"  --port 7102 --save "" --appendonly no
[ -n "$VALKEY" ]    && bench_server valkey    redis    7103 "$VALKEY" --port 7103 --save "" --appendonly no
[ -n "$KEYDB" ]     && bench_server keydb     redis    7104 "$KEYDB"  --port 7104 --save "" --appendonly no --server-threads "$SHARDS"
[ -n "$MEMCACHED" ] && bench_server memcached memcache_text 7105 "$MEMCACHED" -p 7105 -t "$SHARDS" -m 2048

echo
echo "================ SUMMARY (ops/sec, higher is better) ================"
printf "%-12s" "pipeline"
for n in "${NAMES[@]}"; do printf "%14s" "$n"; done; echo
for p in "${PIPES[@]}"; do
  printf "P=%-10s" "$p"
  for n in "${NAMES[@]}"; do printf "%14s" "$(lookup "$n" "$p")"; done
  echo
done
rm -f "$RESFILE"
