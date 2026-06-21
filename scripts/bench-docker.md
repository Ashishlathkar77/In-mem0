# Benchmarking against Dragonfly and Garnet (Linux / Docker)

`scripts/bench-all.sh` covers Redis, Valkey, KeyDB, and Memcached natively. Dragonfly (Linux-only)
and Microsoft Garnet (.NET) don't run natively on macOS — run them via Docker on a Linux host and
point `memtier_benchmark` at them with the same parameters for an apples-to-apples comparison.

Both speak RESP, so the exact same memtier invocation as the script's RESP path applies.

## Dragonfly

```bash
docker run --rm -p 7106:6379 --ulimit memlock=-1 \
  docker.dragonflydb.io/dragonflydb/dragonfly --logtostderr

memtier_benchmark -s 127.0.0.1 -p 7106 -P redis \
  -t 4 -c 25 -n 200000 --pipeline 64 --ratio 1:1 --data-size 64 \
  --key-maximum 1000000 --hide-histogram
```

## Garnet

```bash
docker run --rm -p 7107:6379 ghcr.io/microsoft/garnet --port 6379

memtier_benchmark -s 127.0.0.1 -p 7107 -P redis \
  -t 4 -c 25 -n 200000 --pipeline 64 --ratio 1:1 --data-size 64 \
  --key-maximum 1000000 --hide-histogram
```

## Notes on fair comparison

- Run all systems on the **same host**, pinned to the same cores, with persistence disabled.
- Dragonfly and Garnet are thread-per-core / multi-threaded and shine at high core counts — this
  is exactly the regime inmem targets, so compare on a representative multi-core box.
- For tail latency, add `--hdr-file-prefix` to memtier and compare p50/p99/p99.9, and prefer a
  closed-loop-aware setup (memtier models this better than `redis-benchmark`).
