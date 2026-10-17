#!/bin/sh
# Real-workspace benchmark for baztet, using hyperfine.
#
# Usage:
#   BAZTET_REAL_WS=/path/to/workspace \
#   BAZTET_REAL_PV=/path/to/.bazelproject (optional) \
#   scripts/bench-real.sh [binary]
#
# Cold syncs (fresh/full aspect re-analysis) are single measurements —
# Bazel-side cache/server state makes repetition meaningless. Everything
# cacheable gets warmup + >=5 runs. Results land in docs/bench-*.md and
# stdout.
set -eu

BIN="${1:-./target/release/baztet}"
WS="${BAZTET_REAL_WS:?set BAZTET_REAL_WS to a real workspace}"
PV="${BAZTET_REAL_PV:-}"
PV_ARGS=""
[ -n "$PV" ] && PV_ARGS="--project-view $PV"

mkdir -p docs
echo "== version"; "$BIN" --version

echo "== sync --fresh (cold, 1 run)"
/usr/bin/time -p "$BIN" --workspace "$WS" $PV_ARGS sync --fresh 2>&1 | tail -3

for mode in fast incremental; do
  hyperfine --warmup 2 --min-runs 5 \
    --export-markdown "docs/bench-${mode}.md" \
    --show-output \
    -n "sync" "$BIN --workspace $WS $PV_ARGS sync" >/dev/null
  echo "== sync (${mode})"
  grep -E '\|.*(0\.[0-9]+|[0-9]+)' "docs/bench-${mode}.md" | head -2
done

hyperfine --warmup 1 --min-runs 5 \
  --export-markdown "docs/bench-query.md" \
  -n "sync-fast"  "$BIN --workspace $WS $PV_ARGS sync --fast" \
  -n "targets"    "$BIN --workspace $WS targets" \
  -n "doctor"     "$BIN --workspace $WS doctor" >/dev/null
echo "== query paths"; grep -E '\|.*(0\.[0-9]+|[0-9]+)' docs/bench-query.md
chmod +x scripts/bench-real.sh 2>/dev/null || true
