#!/usr/bin/env bash
# One-shot setup for a fresh Ubuntu/Debian Linux box (a free local multipass VM, an Oracle
# always-free instance, or any cloud box). Installs the toolchain + competitor caches, builds
# inmem, and runs the multi-framework benchmark. Idempotent — safe to re-run.
#
#   curl/scp this file onto the box (or use it from a mounted repo), then:
#     bash scripts/provision-linux.sh
#
# Linux gives us io_uring (kernel 5.11+) — check yours with `uname -r`.
set -euo pipefail

echo "==> kernel: $(uname -r)   cores: $(nproc)"

echo "==> installing system packages (sudo)"
sudo apt-get update -y
sudo apt-get install -y --no-install-recommends \
  build-essential pkg-config libssl-dev git curl ca-certificates \
  redis-server redis-tools valkey-server keydb-server memcached \
  || echo "note: some cache packages may not exist on this distro/release — they'll be skipped in the bench"

# memtier_benchmark: from apt if available, else build from source.
if ! command -v memtier_benchmark >/dev/null 2>&1; then
  if ! sudo apt-get install -y memtier-benchmark 2>/dev/null; then
    echo "==> building memtier_benchmark from source"
    sudo apt-get install -y autoconf automake libpcre3-dev libevent-dev zlib1g-dev
    tmp="$(mktemp -d)"; git clone --depth 1 https://github.com/RedisLabs/memtier_benchmark "$tmp"
    ( cd "$tmp" && autoreconf -ivf && ./configure && make -j"$(nproc)" && sudo make install )
  fi
fi

echo "==> installing Rust"
if ! command -v cargo >/dev/null 2>&1; then
  curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --default-toolchain stable
fi
. "$HOME/.cargo/env"

echo "==> Docker (optional, for Dragonfly/Garnet) — skip if it fails"
if ! command -v docker >/dev/null 2>&1; then
  sudo apt-get install -y docker.io || true
  sudo usermod -aG docker "$USER" || true
  echo "   (log out/in for docker group to take effect, or run docker with sudo)"
fi

echo "==> building inmem (release)"
cargo build --release
# Once ADR-002 lands, also: cargo build --release --features io-uring

echo "==> running the multi-framework benchmark"
./scripts/bench-all.sh "${1:-200000}" "${2:-50}" "${3:-4}"

cat <<'EOF'

==> To include Dragonfly and Garnet (Docker), run them and point memtier at them — see
    scripts/bench-docker.md. Example:
      docker run --rm -d -p 7106:6379 --ulimit memlock=-1 \
        docker.dragonflydb.io/dragonflydb/dragonfly
      memtier_benchmark -p 7106 -P redis -t 4 -c 25 -n 200000 --pipeline 64 \
        --ratio 1:1 --data-size 64 --key-maximum 1000000 --hide-histogram
EOF
