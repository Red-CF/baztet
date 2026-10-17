#!/bin/sh
# baztet installer: downloads the latest GitHub release binary.
# macOS (arm64/intel) and Linux. No config, no cache, one file.
set -eu

REPO="Red-CF/baztet"
BIN="baztet"

case "$(uname -s)" in
  Darwin) PLATFORM="macOS" ;;
  Linux) PLATFORM="Linux" ;;
  *) echo "error: unsupported platform (use cargo or the Windows release)" >&2; exit 1 ;;
esac

case "$(uname -m)" in
  arm64 | aarch64) ARCH="ARM64" ;;
  x86_64) ARCH="X64" ;;
  *) echo "error: unsupported architecture $(uname -m)" >&2; exit 1 ;;
esac

ASSET="${PLATFORM}-${ARCH}"
if [ "$PLATFORM" = "Linux" ]; then
  ASSET="Linux-X64"
fi
URL="https://github.com/${REPO}/releases/latest/download/baztet-${ASSET}"

DEST=""
if [ "$(id -u)" = "0" ]; then
  DEST="/usr/local/bin/${BIN}"
else
  DEST="${HOME}/.local/bin/${BIN}"
  mkdir -p "$(dirname "$DEST")"
fi

echo "downloading baztet (${ASSET}) -> ${DEST}"
curl -fsSL "$URL" -o "$DEST"
chmod +x "$DEST"
echo "installed: $(${DEST} --version 2>/dev/null || echo "$DEST")"
case ":$PATH:" in
  *":$(dirname "$DEST"):"*) ;;
  *) echo "note: $(dirname "$DEST") is not on your PATH" ;;
esac