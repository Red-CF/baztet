# baztet-bep

[![crates.io](https://img.shields.io/crates/v/baztet-bep.svg)](https://crates.io/crates/baztet-bep)

Build Event Protocol (BEP) reader for the [baztet](https://github.com/Red-CF/baztet)
CLI: parses the binary file Bazel writes with `--build_event_binary_file`
(varint-framed `BuildEvent`s) into a queryable [`BepOutput`].

```rust
let output = baztet_bep::parse(&bytes)?;
// per-target aspect outputs, resolved recursively through named sets
let files = output.files_for_target_group(&target_key, "intellij-info");
// build status, server pid, configurations, test summaries, junit xml paths
println!("{:?} -> {}", output.exit_code, output.build_tool_version());
```

Covers: the named-set registry, per-target configuration-scoped output
groups, `bazel config`-style configurations, `BuildStarted` (server pid,
tool version), `BuildFinished` (exit code), and per-attempt `TestResult`
details (`test.xml`/`test.log` output paths) plus `TestSummary` statuses.

You probably want [`baztet`](https://crates.io/crates/baztet), not this.
