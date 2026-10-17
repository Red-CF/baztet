# baztet

[![CI](https://github.com/Red-CF/baztet/actions/workflows/ci.yml/badge.svg)](https://github.com/Red-CF/baztet/actions/workflows/ci.yml)
[![crates.io](https://img.shields.io/crates/v/baztet.svg)](https://crates.io/crates/baztet)

Standalone Bazel sync/execution CLI in Rust — see the
[project README](https://github.com/Red-CF/baztet) for the full picture,
install options, and docs.

```bash
baztet sync     # .bazelproject -> snapshot (query + aspect phases)
baztet targets  # browse synced targets
baztet test     # run your tests with BEP results and junit reports
```

Install: `cargo install baztet` (or the [release binaries](https://github.com/Red-CF/baztet/releases)).
