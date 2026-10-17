# baztet

Standalone, IDE-free CLI for Bazel projects, reimplementing the core engine
of JetBrains' [hirschgarten](https://github.com/JetBrains/hirschgarten) (the
IntelliJ Bazel plugin) as one fast Rust binary.

License: Apache-2.0. Vendored protos and the JetBrains intellij-aspect sources
are Apache-2.0; see `third_party/THIRD_PARTY_NOTICE.md`.

## Status (M1)

Phase-1 project sync works end to end:

```sh
baztet sync          # query phase -> snapshot (add aspect phase: default; --fast skips it)
baztet targets       # list synced targets (filter, --json)
baztet deps LABEL    # direct/transitive deps, or --reverse
```

Executed flows (M3, mirrors the plugin's run configurations and ExecuteService):

```sh
baztet build [labels]        # bazel build + project view build_flags
baztet test [labels]         # --test_filter, PASSED/FAILED per target,
                             # --cases prints failing junit cases, --json report
baztet run LABEL             # bazel run with streamed output
baztet coverage [labels]     # lcov report paths
baztet recipes [--run NAME]  # imported run configs (.bazel/*.xml -> command, target, env)
```

## M4 extras

- **Incremental sync**: unchanged project view + unchanged BUILD files →
  snapshot reused as-is (sub-second re-sync). Aspect outputs reused per-file
  by path+mtime.
- **Sharding**: `shard_sync: true` + `target_shard_size: N` batch the query
  lexicographically with OOM-halving on `java heap` failures.
- **Configurations**: `bazel config --dump_all --output=json` with BEP
  fallback.
- **Doctor**: `baztet doctor` checks bazel, workspace, project view,
  snapshot, aspect deployment.
- **Bench**: `baztet bench --targets N` — e.g. 200k frames decode in ~15 ms,
  10k-target snapshot roundtrip ~5 ms.

## Honest performance note

Bazel itself dominates sync and build wall-clock time. baztet's wins are
process-level: no JVM/IDE startup, streaming proto decoding (no giant lists
in heap), parallel aspect-output parsing, and one static binary.

## Install

```sh
cargo install baztet
```

`baztet` es el único comando público: un binario con toda la funcionalidad.
Los demás crates de este workspace (`baztet-core`, `baztet-protos`, ...)
son dependencias internas de implementación y no hay que instalarlos a mano.

## Building

```sh
cargo build --release   # o `cargo install --path crates/baztet` local
```

Requirements: Rust; bazel 8/9 on PATH for running against a workspace.

## Development

```sh
cargo test                          # unit tests
cargo test -p baztet -- --ignored   # integration tests (requires bazel)
```