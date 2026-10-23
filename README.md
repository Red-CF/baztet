# baztet 🐈‍⬛

[![CI](https://github.com/Red-CF/baztet/actions/workflows/ci.yml/badge.svg)](https://github.com/Red-CF/baztet/actions/workflows/ci.yml)
[![coverage](https://raw.githubusercontent.com/Red-CF/baztet/gh-pages/coverage.svg)](https://github.com/Red-CF/baztet/actions/workflows/ci.yml)
[![crates.io](https://img.shields.io/crates/v/baztet.svg)](https://crates.io/crates/baztet)
[![license: Apache-2.0](https://img.shields.io/badge/license-Apache--2.0-blue.svg)](LICENSE)
[![macOS ARM64](https://img.shields.io/badge/macOS-ARM64-999999.svg)](https://github.com/Red-CF/baztet/releases)
[![macOS Intel](https://img.shields.io/badge/macOS-Intel-999999.svg)](https://github.com/Red-CF/baztet/releases)
[![Linux](https://img.shields.io/badge/Linux-X64-fcc624.svg)](https://github.com/Red-CF/baztet/releases)
[![Windows](https://img.shields.io/badge/Windows-X64-0078d6.svg)](https://github.com/Red-CF/baztet/releases)

`baztet` is a standalone Rust CLI for Bazel projects: one static binary that
syncs your project, runs `build`/`test`/`run`/`coverage`, and reports
results. It is the engine of JetBrains'
[hirschgarten](https://github.com/JetBrains/hirschgarten) (the IntelliJ Bazel
plugin) reimplemented for the terminal. No IDE, no JVM: baztet talks to Bazel
the same way the plugin does — streamed query protos, the `intellij-aspect`,
the Build Event Protocol — and decodes everything itself, in Rust.

It understands the plugin's world: `.bazelproject` project views (including
the sections the plugin generates: `test_sources`, `workspace_type`,
`import_run_configurations`, ...), your `.bazel/*.xml` run configurations,
test filters, and sharded syncs.

### The name

**baz** + **tet**: a wrapper around the Bazel CLI that makes the work as
simple as the IDE's one sync button, from any terminal, with no IDE
attached. And **Bastet**, the Egyptian goddess, always shown as a domestic
cat — the tool stays out of your way and pounces on exactly what changed.

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

## Concepts (2-minute read)

If you know the IntelliJ Bazel plugin, skip this: baztet uses the same
vocabulary and the same files.

- **Project view** (`.bazelproject`, same format as the plugin): what to
  sync. `targets:` lists the labels to include, `directories:` expands to
  every package in a directory tree, `targets:` entries starting with `-`
  exclude. Optional flags (`build_flags`, `sync_flags`) are passed through
  to Bazel.
- **Query phase**: `baztet` runs `bazel query --output=streamed_proto` on
  those targets and builds the **target graph** (labels, rule kinds,
  dependencies, sources). Small, fast, no analysis.
- **Aspect phase**: baztet deploys the vendored JetBrains
  `intellij-aspect` into `<workspace>/.baztet/aspect/` and runs
  `bazel build --aspects` on the synced targets. The aspect emits one
  `TargetIdeInfo` summary per target (dependencies, sources, java jars);
  Bazel hands baztet those files through the **Build Event Protocol** (BEP,
  a binary event stream written to a sidecar file).
- **Snapshot**: everything the two phases produced (graph + ide summaries),
  serialized bincode+zstd into one file under the platform cache dir, keyed
  by the canonical workspace path. Every later command (`targets`, `deps`,
  `test`, ...) loads it; `bazel info` results are cached alongside so
  commands don't spawn Bazel twice.

## Getting oriented

```bash
baztet sync     # project view -> target graph snapshot (fast query phase)
                #   + aspect phase for full dependency/source data
baztet targets  # browse the synced targets
baztet doctor   # self-check: bazel, workspace, project view, snapshot, aspect
```

No configuration is required: baztet auto-discovers the workspace by
walking up from your current directory, and the project view among the
known locations (`.bazelproject`, then the JetBrains BSP server's
`.bazelbsp/.bazelproject`). `--workspace` / `--project-view` override
either one; `baztet doctor` prints what it found.

Then your daily flows:

```bash
baztet build [labels]                  # bazel build with your project view flags
baztet test [labels]                   # BEP-driven results, junit case reports
baztet test --pattern unit             # select tests by label substring
baztet test                            # live dashboard on a terminal; q cancels bazel
baztet test --cases                    # per-case results for failures/errors
baztet test --json                     # machine-readable report (targets + cases)
baztet run LABEL                       # streamed output
baztet coverage [labels]               # merged lcov report paths
baztet recipes                         # your .bazel/*.xml run configs, imported
baztet recipes --run "web unit tests"  # run one, with its env vars
```

Anything that is not a baztet command is passed straight through to
bazel in the discovered workspace: `baztet cquery //pkg:...`,
`baztet aquery ...` — baztet is a drop-in replacement, not a smaller
language.

## Output: quiet by default

On a terminal, `baztet test` runs the live dashboard (per-target table, q
cancels). `build`/`test`/`coverage` without a dashboard capture bazel's
console output and show signal instead of noise: a Bastet spinner while
bazel works, every warning line after it finishes, and the raw error
output only when something fails. A green build is one line. `--raw`
restores full streaming (and disables the dashboard); pipes and CI get
the capture automatically; `run` stays interactive by definition.

Exit codes keep their contract: `test` exits 3 when any test target did
not pass (FLAKY / CANCELLED / TIMEOUT included), 0 otherwise — for CI to
gate on. Passthrough executes forward bazel's own exit code untouched.
`baztet targets --filter =java_library` selects by rule kind (the `=`
prefix); plain filters match label substrings.

## How sync stays fast

Sync is incremental in three steps, cheapest first. Each step compares
against the previous snapshot; the first mismatch falls through to the
next one.

1. **Fingerprint of the project view**: same targets/excludes/flags/Bazel
   version → the query result is reused, no Bazel run at all.
2. **BUILD-file mtimes**: nothing changed since the last sync → the whole
   snapshot is reused (sub-second re-sync). This is the common case.
3. **Delta aspect**: a BUILD file changed in some packages → Bazel
   re-queries (cheap) and the aspect build runs **only on the targets of
   changed packages**; unaffected targets keep their aspect outputs and
   their cache, and nothing is re-parsed.

The snapshot lives OUTSIDE your workspace, zero git noise — and the aspect
dir is self-ignored (`.baztet/aspect/.gitignore`):

```
~/Library/Caches/baztet/<workspace-hash>/snapshot.bin    # macOS
~/.cache/baztet/<workspace-hash>/snapshot.bin            # Linux
%LOCALAPPDATA%/baztet/.../snapshot.bin                    # Windows
```

Override the location with `--cache-dir` or the `BAZTET_CACHE_DIR` env var.
bincode+zstd, a few hundred KB for a thousand-target scope.

Useful flags:

- `--fresh`: skip step 1 and 2, full re-query.
- `--fast`: stop after the query phase (skip the aspect build; faster, and
  enough when you only need targets/dependencies, not IDE data).
- `--no-output-check`: pass `--noexperimental_check_output_files` to Bazel
  so warm builds skip its output-file freshness scan. Opt-in on purpose:
  Bazel trusts outputs that are present on disk, so run a checked build
  before publishing artifacts.
- `--raw` (global): stream every command's bazel output live instead of
  the quiet capture.

## Performance

Measured with [hyperfine](https://github.com/sharkdp/hyperfine) against a
real Bazel 9 workspace (~756 targets); full methodology and numbers in
[docs/BENCHMARKS.md](docs/BENCHMARKS.md), reproducible with
`scripts/bench-real.sh`:

| path | time |
|---|---|
| `sync --fresh` (cold, full query + aspect) | ~53 s — ~40 s of that is Bazel's own analysis |
| `sync` with nothing changed | **~0.7 s** |
| `targets` (756 rows) | **~15 ms** |
| decode 200k target frames (`baztet bench`) | ~15 ms |
| snapshot roundtrip, 10k targets | ~5 ms |

Honest note: Bazel itself dominates sync wall-clock — the cold range is
Bazel cache warmth, not baztet. baztet's wins are process-level and
incremental: no IDE/JVM boot, streaming proto decode in near-constant
memory, the reuse ladder above, delta aspect builds, cached `bazel info`,
and per-command latency around the snapshot instead of a Bazel spawn.

## Status

Sync (query + aspect), execution (build/test/run/coverage), run-config
recipes, sharded syncs with OOM-halving, incremental reuse, and a doctor
are implemented and validated against real Bazel 9 workspaces
(see the integration test).

Not implemented (yet): JVM/Starlark debug protocols, IDE editor features
(those live in the plugin by definition).

## Development

Crates, what each is for, where to start reading:

| crate | role |
|---|---|
| `baztet` | the CLI. Modules: `main.rs` = clap definitions + a 10-line dispatcher; `ctx.rs` = the per-command context (workspace/PV/runner/snapshot, built once); `commands.rs` = one function per subcommand; `ui.rs` = stderr presentation (`--json`/machine output never touch it); `tui.rs` = the alt-screen test dashboard. |
| `baztet-core` | the pipeline. Start at `sync::sync` (orchestration); `query.rs` for phase 1 and `reuse.rs` for the reuse ladder (pure, table-tested); `exec.rs` for build/test/run/coverage; `aspect.rs` for deploying the vendored `intellij-aspect`; `ideinfo.rs` decodes the `TargetIdeInfo` text-format protos. |
| `baztet-protos` | prost codegen for the vendored Bazel protos (`blaze_query`, build event stream) — a leaf, no internal deps. |
| `baztet-bep` | BEP binary stream → queryable `BepOutput`. Leaf. |
| `baztet-projectview` | `.bazelproject` parser + model. Leaf. |
| `baztet-snapshot` | `Snapshot` (schema versioned), bincode+zstd roundtrip, cache-dir location. Leaf. |

Error convention: query failures abort `sync`; aspect/BEP failures
degrade — the snapshot saves what was parsed and `has_error` is set.

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
