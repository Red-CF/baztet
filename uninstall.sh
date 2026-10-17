#!/bin/sh
# removes the baztet binary installed by install.sh
set -eu
BIN="baztet"

for dir in /usr/local/bin "${HOME}/.local/bin"; do
  if [ -x "${dir}/${BIN}" ]; then
    rm -f "${dir}/${BIN}"
    echo "removed ${dir}/${BIN}"
  fi
done

command -v "$BIN" >/dev/null 2>&1 \
  && echo "note: a $BIN is still on your PATH (installed with cargo? use: cargo uninstall $BIN)" \
  || echo "done"