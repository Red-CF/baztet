//! BEP (`--build_event_binary_file`) reader: parses framed BuildEvents into a queryable BepOutput (named sets, per-target output groups, configurations, build status, test summaries).

use std::collections::{BTreeMap, BTreeSet};

use baztet_protos::build_event_stream::{self, build_event_id};
use prost::Message;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum BepError {
    #[error("failed to read BEP file: {0}")]
    Io(#[from] std::io::Error),
    #[error("failed to decode BEP event: {0}")]
    Decode(String),
}

/// Aspect output group names (mirrors the SDK's `OutputGroups`).
pub const OUTPUT_GROUP_INFO: &str = "intellij-info";
pub const OUTPUT_GROUP_SYNC: &str = "intellij-sync";
pub const OUTPUT_GROUP_BUILD: &str = "intellij-build";

/// One file from BEP; `prefix/name` resolves relative to the execution root.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct BepFile {
    pub path_prefix: Vec<String>,
    pub name: String,
}

impl BepFile {
    /// Relative path (bazel-out style), like the plugin's `BazelPathsResolver`
    /// for URI-less files: path_prefix joined with name.
    pub fn relative_path(&self) -> String {
        if self.path_prefix.is_empty() {
            return self.name.clone();
        }
        format!("{}/{}", self.path_prefix.join("/"), self.name)
    }
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct NamedSet {
    pub files: Vec<BepFile>,
    pub file_sets: Vec<String>,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Configuration {
    pub mnemonic: String,
    pub cpu: String,
    pub platform_name: String,
    pub is_tool: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, PartialOrd, Ord)]
pub struct TargetKey {
    pub label: String,
    pub configuration: String,
}

#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct TestSummary {
    pub label: String,
    /// BEP status enum discriminant as text (PASSED, FAILED, …)
    pub status: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct TestResultDetail {
    pub label: String,
    /// BEP status enum discriminant as text (PASSED, FAILED, …)
    pub status: String,
    pub cached_locally: bool,
    /// `test.xml` output, when present
    pub xml: Option<BepFile>,
    /// `test.log` output, when present
    pub log: Option<BepFile>,
}

#[derive(Debug, Clone, Default)]
pub struct BepOutput {
    pub named_sets: BTreeMap<String, NamedSet>,
    /// target -> configuration -> output group -> named set ids
    pub target_outputs: BTreeMap<TargetKey, BTreeMap<String, Vec<String>>>,
    pub configurations: BTreeMap<String, Configuration>,
    pub server_pid: Option<u32>,
    pub build_tool_version: Option<String>,
    /// exit code of the whole build (0 = success)
    pub exit_code: Option<i32>,
    pub test_summaries: Vec<TestSummary>,
    /// per-attempt results (from TestResult events)
    pub test_results: Vec<TestResultDetail>,
    /// target label -> success flag of its latest TargetCompleted event
    /// (live build progress for the dashboard)
    pub target_status: BTreeMap<String, bool>,
    /// non-empty stderr lines from progress events (diagnostics source)
    pub progress_lines: Vec<String>,
}

impl BepOutput {
    /// Merge a parsed delta into the running state. Repeated events win:
    /// test summaries and target status are keyed by label, the rest append.
    pub fn extend(&mut self, delta: BepOutput) {
        for s in delta.test_summaries {
            self.test_summaries.retain(|t| t.label != s.label);
            self.test_summaries.push(s);
        }
        for (key, groups) in delta.target_outputs {
            let buckets = self.target_outputs.entry(key).or_default();
            for (group, sets) in groups {
                buckets.entry(group).or_default().extend(sets);
            }
        }
        self.named_sets.extend(delta.named_sets);
        self.configurations.extend(delta.configurations);
        self.target_status.extend(delta.target_status);
        self.test_results.extend(delta.test_results);
        self.progress_lines.extend(delta.progress_lines);
        self.server_pid = delta.server_pid.or(self.server_pid);
        self.build_tool_version = match self.build_tool_version.take() {
            v @ Some(_) => delta.build_tool_version.or(v),
            None => delta.build_tool_version,
        };
        self.exit_code = delta.exit_code.or(self.exit_code);
    }

    /// Files of one named set, child sets resolved recursively.
    pub fn resolve_named_set(&self, id: &str) -> BTreeSet<BepFile> {
        self.resolve_many([id])
    }

    pub fn resolve_many(
        &self,
        ids: impl IntoIterator<Item = impl AsRef<str>>,
    ) -> BTreeSet<BepFile> {
        fn walk(
            out: &BepOutput,
            id: &str,
            seen: &mut BTreeSet<String>,
            files: &mut BTreeSet<BepFile>,
        ) {
            if !seen.insert(id.to_string()) {
                return;
            }
            let Some(set) = out.named_sets.get(id) else {
                return;
            };
            files.extend(set.files.iter().cloned());
            for child in &set.file_sets {
                walk(out, child, seen, files);
            }
        }
        let mut files = BTreeSet::new();
        let mut seen = BTreeSet::new();
        for id in ids {
            walk(self, id.as_ref(), &mut seen, &mut files);
        }
        files
    }

    pub fn files_for_target_group(&self, target: &TargetKey, group: &str) -> BTreeSet<BepFile> {
        let Some(groups) = self.target_outputs.get(target) else {
            return Default::default();
        };
        let Some(sets) = groups.get(group) else {
            return Default::default();
        };
        self.resolve_many(sets.iter().map(|s| s.as_str()))
    }
}

/// Frames from `start` until the first incomplete one (a mid-write tail is
/// normal). Returns the delta plus how many bytes it consumed; the caller
/// feeds `start` back as its cursor so growth-only parsing stays O(new bytes).
pub fn parse_prefix(bytes: &[u8], start: usize) -> Result<(BepOutput, usize), BepError> {
    let start = start.min(bytes.len());
    let mut out = BepOutput::default();
    let mut cursor = &bytes[start..];
    let mut consumed = 0usize;
    while !cursor.is_empty() {
        let before = cursor.len();
        match build_event_stream::BuildEvent::decode_length_delimited(&mut cursor) {
            Ok(event) => handle(&mut out, event)?,
            // incomplete or corrupt frame: stop at the last good boundary
            Err(_) => break,
        }
        consumed += before - cursor.len();
    }
    Ok((out, start + consumed))
}

/// Parse a complete BEP file's bytes (length-delimited `BuildEvent` frames).
pub fn parse(bytes: &[u8]) -> Result<BepOutput, BepError> {
    let mut out = BepOutput::default();
    let mut cursor = bytes;
    while !cursor.is_empty() {
        let event = build_event_stream::BuildEvent::decode_length_delimited(&mut cursor)
            .map_err(|e| BepError::Decode(e.to_string()))?;
        handle(&mut out, event)?;
    }
    Ok(out)
}

fn handle(out: &mut BepOutput, event: build_event_stream::BuildEvent) -> Result<(), BepError> {
    use build_event_stream::build_event::Payload as P;
    use build_event_stream::build_event_id::Id as IdKind;

    let id_kind = event.id.and_then(|i| i.id);

    match event.payload {
        Some(P::Started(started)) => {
            out.server_pid = (started.server_pid > 0).then_some(started.server_pid as u32);
            out.build_tool_version = Some(started.build_tool_version);
        }
        Some(P::NamedSetOfFiles(ns)) => {
            let Some(IdKind::NamedSet(build_event_id::NamedSetOfFilesId { id })) = id_kind else {
                return Ok(());
            };
            let set = NamedSet {
                files: ns
                    .files
                    .iter()
                    .map(|f| BepFile {
                        path_prefix: f.path_prefix.clone(),
                        name: f.name.clone(),
                    })
                    .collect(),
                file_sets: ns.file_sets.iter().map(|n| n.id.clone()).collect(),
            };
            out.named_sets.insert(id, set);
        }
        Some(P::Completed(completed)) => {
            let Some(IdKind::TargetCompleted(build_event_id::TargetCompletedId {
                label,
                configuration,
                ..
            })) = id_kind
            else {
                return Ok(());
            };
            let key = TargetKey {
                label: label.clone(),
                configuration: configuration.map(|c| c.id).unwrap_or_default(),
            };
            let groups = out.target_outputs.entry(key).or_default();
            for group in &completed.output_group {
                groups
                    .entry(group.name.clone())
                    .or_default()
                    .extend(group.file_sets.iter().map(|fs| fs.id.clone()));
            }
            // aspect completions re-report the base target; last wins is fine
            // for the dashboard (both carry the same success flag)
            out.target_status.insert(label, completed.success);
        }
        Some(P::Configuration(config)) => {
            let Some(IdKind::Configuration(build_event_id::ConfigurationId { id })) = id_kind
            else {
                return Ok(());
            };
            out.configurations.insert(
                id,
                Configuration {
                    mnemonic: config.mnemonic,
                    cpu: config.cpu,
                    platform_name: config.platform_name,
                    is_tool: config.is_tool,
                },
            );
        }
        Some(P::Finished(finished)) => {
            out.exit_code = finished.exit_code.map(|c| c.code).or(out.exit_code);
        }
        Some(P::TestResult(result)) => {
            let Some(IdKind::TestResult(build_event_id::TestResultId { label, .. })) = id_kind
            else {
                return Ok(());
            };
            let find = |name: &str| {
                result
                    .test_action_output
                    .iter()
                    .find(|f| f.name == name)
                    .map(|f| BepFile {
                        path_prefix: f.path_prefix.clone(),
                        name: f.name.clone(),
                    })
            };
            out.test_results.push(TestResultDetail {
                label,
                status: format_test_status(result.status),
                cached_locally: result.cached_locally,
                xml: find("test.xml"),
                log: find("test.log"),
            });
        }
        Some(P::TestSummary(summary)) => {
            let Some(IdKind::TestSummary(build_event_id::TestSummaryId { label, .. })) = id_kind
            else {
                return Ok(());
            };
            out.test_summaries.push(TestSummary {
                label,
                status: format_test_status(summary.overall_status),
            });
        }
        Some(P::Progress(progress)) => {
            let stderr = &progress.stderr;
            for line in stderr.lines() {
                let line = line.trim();
                if !line.is_empty() {
                    out.progress_lines.push(strip_ansi(line));
                }
            }
        }
        _ => {}
    }
    Ok(())
}

fn format_test_status(status: i32) -> String {
    use build_event_stream::TestStatus;
    Some(status)
        .and_then(|s| TestStatus::try_from(s).ok())
        .map(|s| {
            match s {
                TestStatus::NoStatus => "NO_STATUS",
                TestStatus::Passed => "PASSED",
                TestStatus::Flaky => "FLAKY",
                TestStatus::Timeout => "TIMEOUT",
                TestStatus::Failed => "FAILED",
                TestStatus::Incomplete => "INCOMPLETE",
                TestStatus::RemoteFailure => "REMOTE_FAILURE",
                TestStatus::FailedToBuild => "FAILED_TO_BUILD",
                TestStatus::ToolHaltedBeforeTesting => "TOOL_HALTED_BEFORE_TESTING",
            }
            .to_string()
        })
        .unwrap_or_else(|| format!("UNKNOWN({status})"))
}

fn strip_ansi(line: &str) -> String {
    let mut out = String::with_capacity(line.len());
    let mut in_escape = false;
    for c in line.chars() {
        match c {
            '\u{1b}' => in_escape = true,
            _ if in_escape => {
                if c == 'm' || c == 'K' || c == 'H' {
                    in_escape = false;
                }
            }
            _ => out.push(c),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use baztet_protos::build_event_stream as bes;
    use prost::Message;

    fn framed(events: &[bes::BuildEvent]) -> Vec<u8> {
        let mut stream = vec![];
        for e in events {
            e.encode_length_delimited(&mut stream).unwrap();
        }
        stream
    }

    #[test]
    fn named_sets_and_target_groups() {
        use bes::{build_event, build_event_id};

        let ns1_event = bes::BuildEvent {
            id: Some(bes::BuildEventId {
                id: Some(build_event_id::Id::NamedSet(
                    build_event_id::NamedSetOfFilesId { id: "set1".into() },
                )),
            }),
            payload: Some(build_event::Payload::NamedSetOfFiles(
                bes::NamedSetOfFiles {
                    files: vec![bes::File {
                        name: "foo.intellij-info.txt".into(),
                        ..Default::default()
                    }],
                    ..Default::default()
                },
            )),
            ..Default::default()
        };

        let ns2_event = bes::BuildEvent {
            id: Some(bes::BuildEventId {
                id: Some(build_event_id::Id::NamedSet(
                    build_event_id::NamedSetOfFilesId { id: "set2".into() },
                )),
            }),
            payload: Some(build_event::Payload::NamedSetOfFiles(
                bes::NamedSetOfFiles {
                    file_sets: vec![build_event_id::NamedSetOfFilesId { id: "set1".into() }],
                    ..Default::default()
                },
            )),
            ..Default::default()
        };

        let tc_event = bes::BuildEvent {
            id: Some(bes::BuildEventId {
                id: Some(build_event_id::Id::TargetCompleted(
                    build_event_id::TargetCompletedId {
                        label: "//a:b".into(),
                        configuration: Some(build_event_id::ConfigurationId { id: "cfg1".into() }),
                        aspect: String::new(),
                    },
                )),
            }),
            payload: Some(build_event::Payload::Completed(bes::TargetComplete {
                output_group: vec![bes::OutputGroup {
                    name: OUTPUT_GROUP_INFO.into(),
                    file_sets: vec![build_event_id::NamedSetOfFilesId { id: "set2".into() }],
                    ..Default::default()
                }],
                ..Default::default()
            })),
            ..Default::default()
        };

        let output = parse(&framed(&[ns1_event, ns2_event, tc_event])).unwrap();
        let key = TargetKey {
            label: "//a:b".into(),
            configuration: "cfg1".into(),
        };
        let files = output.files_for_target_group(&key, OUTPUT_GROUP_INFO);
        assert_eq!(
            files,
            [BepFile {
                path_prefix: vec![],
                name: "foo.intellij-info.txt".into()
            }]
            .into_iter()
            .collect()
        );
    }

    #[test]
    fn started_and_finished() {
        use bes::{build_event, build_event_id, BuildFinished, BuildStarted};
        let started = bes::BuildEvent {
            id: Some(bes::BuildEventId {
                id: Some(build_event_id::Id::Started(
                    build_event_id::BuildStartedId {},
                )),
            }),
            payload: Some(build_event::Payload::Started(BuildStarted {
                build_tool_version: "release 9.2.0".into(),
                server_pid: 4242,
                ..Default::default()
            })),
            ..Default::default()
        };
        let finished = bes::BuildEvent {
            id: Some(bes::BuildEventId {
                id: Some(build_event_id::Id::BuildFinished(
                    build_event_id::BuildFinishedId {},
                )),
            }),
            payload: Some(build_event::Payload::Finished(BuildFinished {
                exit_code: Some(bes::build_finished::ExitCode {
                    code: 3,
                    name: String::new(),
                }),
                ..Default::default()
            })),
            ..Default::default()
        };
        let out = parse(&framed(&[started, finished])).unwrap();
        assert_eq!(out.server_pid, Some(4242));
        assert_eq!(out.build_tool_version.as_deref(), Some("release 9.2.0"));
        assert_eq!(out.exit_code, Some(3));
    }

    #[test]
    fn strip_ansi_codes() {
        assert_eq!(strip_ansi("\u{1b}[31merr\u{1b}[0m"), "err");
        assert_eq!(strip_ansi("plain"), "plain");
    }

    fn started_event() -> bes::BuildEvent {
        use bes::{build_event, build_event_id, BuildStarted};
        bes::BuildEvent {
            id: Some(bes::BuildEventId {
                id: Some(build_event_id::Id::Started(
                    build_event_id::BuildStartedId {},
                )),
            }),
            payload: Some(build_event::Payload::Started(BuildStarted {
                build_tool_version: "release 9.2.0".into(),
                server_pid: 4242,
                ..Default::default()
            })),
            ..Default::default()
        }
    }

    #[test]
    fn prefix_then_extend_reaches_full_parse() {
        let bytes = framed(&[started_event()]);
        let full = parse(&bytes).unwrap();

        // a prefix stops at the mid-write tail without error
        let (delta, consumed) = parse_prefix(&bytes[..bytes.len() / 2], 0).unwrap();
        let mut state = BepOutput::default();
        state.extend(delta);

        // resuming from the cursor reaches the end
        let (rest, tail) = parse_prefix(&bytes, consumed).unwrap();
        assert_eq!(tail, bytes.len());
        state.extend(rest);
        assert_eq!(state.server_pid, full.server_pid);

        // an exhausted cursor yields an empty delta
        let (empty, again) = parse_prefix(&bytes, bytes.len()).unwrap();
        assert_eq!(again, bytes.len());
        assert!(empty.test_summaries.is_empty() && empty.named_sets.is_empty());
    }
}
