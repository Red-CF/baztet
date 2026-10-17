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

## Honest performance note

Bazel itself dominates sync and build wall-clock time. baztet's wins are
process-level: no JVM/IDE startup, streaming proto decoding (no giant lists
in heap), parallel aspect-output parsing, and one static binary.

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