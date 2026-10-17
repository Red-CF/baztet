# baztet-projectview

[![crates.io](https://img.shields.io/crates/v/baztet-projectview.svg)](https://crates.io/crates/baztet-projectview)

Parser and model for the `.bazelproject` project view format used by
JetBrains' hirschgarten Bazel plugin and the
[baztet](https://github.com/Red-CF/baztet) CLI — written from the format
specification, no IDE dependencies.

Supports: section headers with indented entries, inline scalars (`key:
value`), `-` exclude markers for targets/directories, `#` comments,
`import`/`try_import` (workspace-root-relative), and the plugin's sections
(`targets`, `directories`, flag sections, `test_sources`, `workspace_type`,
`java_language_level`, `import_run_configurations`, `shard_sync`,
`target_shard_size`, ...). Unknown section keys are rejected.

```rust
let view = baztet_projectview::parse_text(
    "derive_targets_from_directories: true\n\ndirectories:\n    pkg/api\n",
)?;
assert!(view.derive_targets_from_directories());
assert_eq!(view.directories.entries[0].as_target(), "//pkg/api:all");
```

You probably want [`baztet`](https://crates.io/crates/baztet), not this.
