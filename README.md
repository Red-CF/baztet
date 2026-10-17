# baztet 🦌

[![CI](https://github.com/Red-CF/baztet/actions/workflows/ci.yml/badge.svg)](https://github.com/Red-CF/baztet/actions/workflows/ci.yml)
[![coverage](https://raw.githubusercontent.com/Red-CF/baztet/gh-pages/coverage.svg)](https://github.com/Red-CF/baztet/actions/workflows/ci.yml)
[![crates.io](https://img.shields.io/crates/v/baztet.svg)](https://crates.io/crates/baztet)
[![downloads](https://img.shields.io/crates/d/baztet.svg)](https://crates.io/crates/baztet)
[![macOS ARM64](https://img.shields.io/badge/macOS-ARM64-999999.svg)](https://github.com/Red-CF/baztet/releases)
[![macOS Intel](https://img.shields.io/badge/macOS-Intel-999999.svg)](https://github.com/Red-CF/baztet/releases)
[![Linux](https://img.shields.io/badge/Linux-X64-fcc624.svg)](https://github.com/Red-CF/baztet/releases)
[![Windows](https://img.shields.io/badge/Windows-X64-0078d6.svg)](https://github.com/Red-CF/baztet/releases)

`baztet` is a standalone Rust CLI for Bazel projects: one static binary that
syncs your project, runs `build`/`test`/`run`/`coverage`, and reports
results — the engine of JetBrains'
[hirschgarten](https://github.com/JetBrains/hirschgarten) (the IntelliJ Bazel
plugin) reimplemented for the terminal. No IDE, no JVM to host the tool:
baztet talks to Bazel the same way the plugin does (streamed query protos,
the `intellij-aspect`, the Build Event Protocol) and decodes everything
itself, in Rust.

It understands the plugin's world: `.bazelproject` project views (including
the sections the plugin generates: `test_sources`, `workspace_type`,
`import_run_configurations`, ...), your `.bazel/*.xml` run configurations,
test filters, and sharded syncs.

## Install

With cargo (installs the `baztet` binary; this is the one command you need):

```bash
cargo install baztet
```

The other crates in this workspace (`baztet-core`, `baztet-protos`, ...) are
internal implementation dependencies — cargo pulls them in automatically,
you never install them by hand.

Without cargo (macOS and Linux; downloads the binary from the latest
release):

```bash
curl -fsSL https://raw.githubusercontent.com/Red-CF/baztet/main/install.sh | sh
```

Windows: grab `baztet-Windows-X64.exe` from the
[releases page](https://github.com/Red-CF/baztet/releases) and put it on
your `PATH`.

Or build from source:

```bash
cargo install --path .
```

## Updating and uninstalling

- **Installed with cargo**: update by re-running `cargo install baztet`;
  remove with `cargo uninstall baztet`.
- **Installed with the script**: update by re-running the install script
  (it overwrites the binary with the latest release); remove with:

```bash
curl -fsSL https://raw.githubusercontent.com/Red-CF/baztet/main/uninstall.sh | sh
```

baztet installs exactly one file, a static binary (`/usr/local/bin/baztet`
or `~/.local/bin/baztet`). No config, cache, or daemon anywhere: deleting
that file is a complete removal. Windows: delete the `.exe` from your
`PATH`.

## Getting oriented

```bash
baztet sync     # project view -> target graph snapshot (fast query phase)
                #   + aspect phase for full dependency/source data
baztet targets  # browse the synced targets
baztet doctor   # self-check: bazel, workspace, project view, snapshot, aspect
```

Then your daily flows:

```bash
baztet build [labels]                  # bazel build with your project view flags
baztet test [labels]                   # BEP-driven results, junit case reports
baztet test --pattern unit             # select tests by label substring
baztet test --json                     # machine-readable report (targets + cases)
baztet run LABEL                       # streamed output
baztet coverage [labels]               # merged lcov report paths
baztet recipes                         # your .bazel/*.xml run configs, imported
baztet recipes --run "web unit tests"  # run one, with its env vars
```

## The snapshot

`sync` writes `<workspace>/.baztet/snapshot.bin` (bincode+zstd, a few hundred
KB for a thousand-target scope). Incremental by default:

- unchanged project view -> the query graph is reused, no re-query
- unchanged BUILD files (max mtime over the package walk) -> the whole
  snapshot is reused, sub-second re-sync
- changed BUILD files -> only modified `*.intellij-info.txt` aspect outputs
  are re-parsed (path+mtime reuse)

`--fresh` forces a full re-query; `--fast` skips the aspect phase.

## Performance

Measured with [hyperfine](https://github.com/sharkdp/hyperfine) against a
real Bazel 9 workspace (~756 targets); full methodology and numbers in
[docs/BENCHMARKS.md](docs/BENCHMARKS.md), reproducible with
`scripts/bench-real.sh`:

| path | time |
|---|---|
| `sync --fresh` (cold, full query + aspect) | ~53 s — ~40 s of that is Bazel's own analysis |
| `sync` after a BUILD edit | ~46 s (re-query, intentional) |
| `sync` with nothing changed | **~0.7 s** |
| `targets` (756 rows) | **~15 ms** |
| decode 200k target frames (`baztet bench`) | ~15 ms |
| snapshot roundtrip, 10k targets | ~5 ms |

Honest note: Bazel itself dominates sync wall-clock. baztet's wins are
process-level: no IDE/JVM boot, streaming proto decode (near-constant
memory), and the reuse ladder — identical inputs never touch Bazel.

## Status

Sync (query + aspect), execution (build/test/run/coverage), run-config
recipes, sharded syncs with OOM-halving, incremental reuse, and a doctor are
implemented and validated against real Bazel 9 workspaces. Honest
performance note: Bazel itself dominates wall-clock time — baztet's wins are
process-level (no IDE/JVM boot, streaming proto decode, parallel aspect
parsing, one static binary; 200k target frames decode in ~15 ms).

Not implemented (yet): JVM/Starlark debug protocols, IDE editor features
(those live in the plugin by definition).

## Development

```bash
cargo test                          # unit tests
cargo test -p baztet -- --ignored   # e2e against the fixture workspace (real bazel)
baztet bench                        # decode + snapshot roundtrip numbers
```

Requirements: Rust; Bazel 8/9 to sync against a workspace. Vendored third
parties (Bazel protos, the JetBrains `intellij-aspect` sources) are
Apache-2.0 wire formats and rules — see
[`crates/baztet-protos/THIRD_PARTY_NOTICE.md`](crates/baztet-protos/THIRD_PARTY_NOTICE.md).
Licensed Apache-2.0 ([LICENSE](LICENSE)).
