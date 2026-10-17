# baztet-protos

[![crates.io](https://img.shields.io/crates/v/baztet-protos.svg)](https://crates.io/crates/baztet-protos)

Vendored Bazel and JetBrains `intellij-aspect` protobufs for the
[baztet](https://github.com/Red-CF/baztet) CLI, compiled with `prost-build`
(with its hermetic `protoc`), plus a `prost-reflect` descriptor pool for the
text-format aspect outputs.

This crate exists so `baztet` can decode what Bazel emits without a codegen
step in your project:

- `gen::blaze_query::Target` — `bazel query --output=streamed_proto` frames
- `gen::build_event_stream::BuildEvent` — Build Event Protocol events
- `gen::TargetIdeInfo` & co — the aspect's per-target info (textproto)
- `embedded_aspect_files()` — the vendored `intellij-aspect` bzl sources the
  deployer writes into the workspace

The vendored files are unmodified Apache-2.0 sources from their upstream
projects; see [THIRD_PARTY_NOTICE.md](THIRD_PARTY_NOTICE.md) for provenance.
You almost certainly do not depend on this crate directly — it is an
implementation detail of [`baztet`](https://crates.io/crates/baztet).
