# baztet-snapshot

[![crates.io](https://img.shields.io/crates/v/baztet-snapshot.svg)](https://crates.io/crates/baztet-snapshot)

Target-graph snapshot model and persistence for the
[baztet](https://github.com/Red-CF/baztet) CLI: what a `baztet sync` produces
and everything behind `baztet targets`/`deps`.

- [`TargetGraph`] — CSR-style graph (labels interned into a `Vec`), dep /
  reverse-dep / transitive queries
- [`Snapshot`] — graph + phase-2 aspect summaries (`IdeSummary`) +
  configurations + sync fingerprints, bincode+zstd at a few hundred KB for a
  thousand-target scope
- incremental fields: per-aspect-output `path@mtime` reuse metadata and the
  max BUILD-file mtime that detects BUILD drift without re-querying

```rust
let snapshot = Snapshot::load(&workspace_root)?;
for node in snapshot.graph.iter() {
    println!("{} [{}]", node.label, node.kind);
}
```

You probably want [`baztet`](https://crates.io/crates/baztet), not this.
