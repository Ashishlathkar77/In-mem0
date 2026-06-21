#!/usr/bin/env bash
# Head-to-head benchmark: inmemd vs redis-server, using redis-benchmark.
#
# Usage: scripts/bench.sh [requests] [clients]
# Requires: redis-benchmark, redis-server on PATH; builds inmemd in release.
set -euo pipefail

REQ="${1:-1000000}"
CLIENTS="${2:-50}"
INMEM_PORT=6390
REDIS_PORT=6391

cd "$(dirname "$0")/.."
cargo build --release

cleanup() { kill "${INMEM:-0}" "${REDIS:-0}" 2>/dev/null || true; }
trap cleanup EXIT

./target/release/inmemd --port "$INMEM_PORT" --shards "$(getconf _NPROCESSORS_ONLN)" >/tmp/inmemd.log 2>&1 &
INMEM=$!
redis-server --port "$REDIS_PORT" --save "" --appendonly no >/tmp/redis.log 2>&1 &
REDIS=$!
sleep 1

for P in 1 16 64; do
  echo "================= pipelining -P $P, $CLIENTS clients, $REQ requests ================="
  echo "----- inmem -----"
  redis-benchmark -p "$INMEM_PORT" -t set,get,incr -n "$REQ" -c "$CLIENTS" -P "$P" -q 2>/dev/null
  echo "----- redis -----"
  redis-benchmark -p "$REDIS_PORT" -t set,get,incr -n "$REQ" -c "$CLIENTS" -P "$P" -q 2>/dev/null
  echo
done
