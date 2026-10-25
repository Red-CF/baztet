# Benchmarks

Environment: macOS darwin (Apple Silicon), Bazel 9.3.0 (warm server),
workspace = a real 9.x bzlmod monorepo, scope = a subpackage tree
(~700-756 targets / ~1,100 ide entries). `hyperfine` for warm paths (warmup 2, >=5
runs), single measurements for cold syncs (Bazel-side state makes repetition
meaningless). Reproduce with:
`BAZTET_REAL_WS=... scripts/bench-real.sh`.

## Sync (the numbers that matter)

| path | time | what it does |
|---|---|---|
| `sync --fresh` (cold) | ~29-53 s wall | query phase + aspect build + BEP parse + ide summaries (parse is rayon-parallel since 0.3.0) |
| `sync` after BUILD edit | ~46 s | same, re-triggered by BUILD mtime (this used to be silently skipped — fixed) |
| `sync` unchanged | **~0.7-0.9 s** | fingerprint + BUILD-mtime check only; snapshot reused whole |
| `sync --fast` | ~0.64 s | reuse path, no aspect phase at all |

The honest reading: sync wall-clock is dominated by Bazel's own analysis;
the cold range (29-53 s) is Bazel cache warmth, not baztet. The baztet-side
win is the **reuse ladder**: identical inputs never touch Bazel. Building
on that, this release narrows what the incremental path gives Bazel:

- delta aspect builds: after a BUILD edit, `--aspects` runs only on the
  targets of changed packages — the affected fraction of the analysis
  window, not all of it
- cached `bazel info` in the snapshot: `targets`/`deps`/`build`/`test`
  stop paying the info spawn
- opt-in `--no-output-check` (`--noexperimental_check_output_files`):
  skips Bazel's output-freshness scan on warm builds

## Query paths (snapshot load)

| command | time |
|---|---|
| `targets` (756 rows) | **~15 ms** |
| `deps` | ~16 ms |
| `sync --fast` (reuse check + save) | ~0.64 s |
| `doctor` | ~0.8 s (includes a `bazel --version` subprocess) |

## Process-level costs

`baztet bench --targets 200000`:

- 200k framed `Build.Target` messages stream-decoded in **~9 ms** (pure varint
  framing + prost; was ~15 ms at 0.3 — mimalloc + allocator-era gains)
- 10k-target snapshot roundtrip (bincode+zstd): **~3 ms** (was ~5 ms at 0.3)

These are the costs the plugin pays inside the IntelliJ JVM for every sync;
baztet pays ~nothing for the equivalent decode paths.

## Where the real wins come from

1. no JVM/IDE process for tool plumbing (startup ~ms vs seconds)
2. streaming decode (constant-ish memory, no giant proto lists)
3. incremental snapshot reuse (sub-second re-syncs during a normal session)
