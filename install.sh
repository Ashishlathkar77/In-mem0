#!/usr/bin/env sh
# Install the inmem cache server (inmemd) — downloads the prebuilt binary for your OS/arch from
# the latest GitHub release. Usage:
#   curl -fsSL https://raw.githubusercontent.com/Ashishlathkar77/In-mem0/main/install.sh | sh
set -eu

REPO="Ashishlathkar77/In-mem0"
BIN="inmemd"
DEST="${INMEM_INSTALL_DIR:-/usr/local/bin}"

os="$(uname -s)"; arch="$(uname -m)"
case "$os" in
  Linux)  os_tag="linux" ;;
  Darwin) os_tag="macos" ;;
  *) echo "unsupported OS: $os (build from source: cargo build --release)"; exit 1 ;;
esac
case "$arch" in
  x86_64|amd64) arch_tag="x86_64" ;;
  arm64|aarch64) arch_tag="aarch64" ;;
  *) echo "unsupported arch: $arch (build from source)"; exit 1 ;;
esac

asset="inmemd-${os_tag}-${arch_tag}"
url="https://github.com/${REPO}/releases/latest/download/${asset}"
echo "Downloading ${asset} ..."
tmp="$(mktemp)"
curl -fSL "$url" -o "$tmp"
chmod +x "$tmp"

if [ -w "$DEST" ]; then
  mv "$tmp" "$DEST/$BIN"
else
  echo "Installing to $DEST (needs sudo)"
  sudo mv "$tmp" "$DEST/$BIN"
fi

echo "Installed $BIN to $DEST/$BIN"
echo "Start it:   $BIN --port 6380"
echo "Connect:    redis-cli -p 6380 ping   (or any Redis client)"
