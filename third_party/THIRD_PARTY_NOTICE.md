# Third-party code notice

All vendored files are unmodified sources from their original projects, both
licensed under the Apache License 2.0 (see LICENSE in each upstream repository).
Copyright for vendored files remains with the original authors.

## Bazel protos (`protos/bazel/`)

- Source: https://github.com/bazelbuild/bazel and https://github.com/JetBrains/hirschgarten
  (`protobuf/src/proto`, version as vendored from hirschgarten HEAD 2026-10).
- Used: build.proto (query), build_event_stream.proto (BEP), action_cache.proto,
  command_line.proto, failure_details.proto, invocation_policy.proto,
  package_load_metrics.proto and their transitive imports.
- Copyright The Bazel Authors / JetBrains s.r.o.

## JetBrains intellij-aspect (`aspect/`)

- Source: https://github.com/JetBrains/intellij-aspect, commit
  `5b08cdf0f02b0a6d4eae9bc96cc7b64a4ac3cd48` (pinned by the hirschgarten build).
- Vendored: `bzl/` (contents of the deployed `archive_ide.zip`: `common/`,
  `intellij/`, `modules/`) and `proto/ide_info.proto`, `proto/common.proto`.
- Copyright JetBrains s.r.o.