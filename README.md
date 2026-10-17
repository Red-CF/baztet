# baztet

Standalone, IDE-free CLI for Bazel projects, reimplementing the core engine
of JetBrains' [hirschgarten](https://github.com/JetBrains/hirschgarten) (the
IntelliJ Bazel plugin) as one fast Rust binary.

License: Apache-2.0. Vendored protos and the JetBrains intellij-aspect sources
are Apache-2.0; see `third_party/THIRD_PARTY_NOTICE.md`.

## Status (M1)

Phase-1 project sync works end to end:

```sh
baztet sync          # project view -> bazel query --output=streamed_proto -> .baztet/snapshot.bin
baztet targets       # list synced targets (filter, --json)
baztet deps LABEL    # direct/transitive deps, or --reverse
```

Planned: phase-2 aspect sync (full dependency/source data), build/test/run
with BEP streaming, coverage, sharding, incremental sync.

## Honest performance note

Bazel itself dominates sync and build wall-clock time. baztet's wins are
process-level: no JVM/IDE startup, streaming proto decoding (no giant lists
in heap), and one static binary. Phase-2 will add parallel aspect-output
parsing and incremental snapshot reuse.

## Building

```sh
cargo build --release
```

Requirements: Rust; bazel 8/9 on PATH for running against a workspace.

## Development

```sh
cargo test                          # unit tests
cargo test -p baztet -- --ignored   # integration tests (requires bazel)
```