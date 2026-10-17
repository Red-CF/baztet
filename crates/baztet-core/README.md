# baztet-core

[![crates.io](https://img.shields.io/crates/v/baztet-core.svg)](https://crates.io/crates/baztet-core)

The sync/execution engine of the [baztet](https://github.com/Red-CF/baztet)
CLI — a Rust reimplementation of the hirschgarten (JetBrains Bazel plugin)
engine, IDE-free:

- **sync** — phase 1: `bazel query --output=streamed_proto` with
  lexicographic sharding and OOM-halving (`shard_sync`, `target_shard_size`);
  phase 2: the vendored `intellij-aspect` deployer (load rewriting, canonical
  repo mapping, config rendering) + BEP consumption + `TargetIdeInfo` parsing
  with per-file incremental reuse
- **exec** — `build`/`test`/`run`/`coverage` with ExecuteService-style flag
  construction, BEP sidecars beside live console output, junit `test.xml`
  case parsing, lcov report discovery
- **recipes** — IntelliJ `BlazeCommandRunConfigurationType` run configs as
  CLI invocations (command, target, env)
- plus workspace discovery, `bazel info` parsing, streaming proto
  framing, and the `doctor`/`bench` internals

You probably want [`baztet`](https://crates.io/crates/baztet), not this.
