//! Execution pipeline: `baztet build/test/run/coverage`, mirroring
//! hirschgarten's `ExecuteService`:
//!
//! - flag sections from the project view (build_flags / test_flags)
//! - `--tool_tag`, `--build_tag_filters=-no-ide`, tty-dependent colors
//! - BEP sidecar file for structured results even with live console output
//! - `--test_filter` passthrough, junit XML parsing from BEP test outputs

use std::collections::BTreeMap;
use std::process::Stdio;

use baztet_bep::{BepError, BepOutput};
use baztet_projectview::ProjectView;
use thiserror::Error;

use crate::bazel::{BazelError, BazelRunner};

#[derive(Debug, Error)]
pub enum ExecError {
    #[error(transparent)]
    Bazel(#[from] BazelError),
    #[error("failed to parse BEP output: {0}")]
    Bep(#[from] BepError),
    #[error(transparent)]
    Io(#[from] std::io::Error),
    /// the bazel command failed (nonzero); result carries the BEP output
    #[error("bazel {command} exited with code {code}")]
    BazelFailed { command: String, code: i32 },
}

/// What to run: labels plus optional pass-through bazel args and env.
#[derive(Debug, Clone, Default)]
pub struct ExecSpec {
    /// target labels (or specs like //pkg:all)
    pub labels: Vec<String>,
    /// passthrough args after `--`
    pub extra_args: Vec<String>,
    pub env: BTreeMap<String, String>,
    /// `--test_filter` value for `test`
    pub test_filter: Option<String>,
}

/// Result of one execution command.
#[derive(Debug, Clone, Default)]
pub struct ExecResult {
    pub code: i32,
    pub bep: Option<BepOutput>,
}

pub const BUILD_TAG_FILTERS: &str = "--build_tag_filters=-no-ide";

fn common_flags(argv: &mut Vec<String>) {
    argv.push("--tool_tag=baztet".to_string());
    argv.push("--color=yes".to_string());
    argv.push("--curses=no".to_string());
    argv.push(BUILD_TAG_FILTERS.to_string());
}

fn bep_flags(bep_file: &std::path::Path, argv: &mut Vec<String>) {
    argv.push(format!("--build_event_binary_file={}", bep_file.display()));
    argv.push("--build_event_binary_file_upload_mode=wait_for_upload_complete".to_string());
    argv.push("--bes_outerr_buffer_size=10".to_string());
}

impl BazelRunner {
    /// `bazel build` on the given labels with the project view's build flags.
    pub async fn exec_build(&self, pv: &ProjectView, spec: &ExecSpec) -> Result<ExecResult, ExecError> {
        self.exec_with_bep("build", pv, spec, |argv| {
            argv.extend(pv.build_flags.iter().cloned());
            Ok(())
        })
        .await
    }

    /// `bazel run` on one label; output is the target's (inherited stdio).
    pub async fn exec_run(&self, pv: &ProjectView, spec: &ExecSpec) -> Result<ExecResult, ExecError> {
        if spec.labels.len() != 1 {
            return Err(BazelError::Other("`run` takes exactly one target".into()).into());
        }
        let mut argv: Vec<String> = vec![];
        common_flags(&mut argv);
        argv.extend(pv.build_flags.iter().cloned());
        let bep_file = temp_bep_file();
        // BEP flags must come BEFORE the target label: everything after the
        // first positional is passed to the executed binary
        bep_flags(&bep_file, &mut argv);
        argv.extend(spec.labels.iter().cloned());
        argv.extend(spec.extra_args.iter().cloned());

        let mut cmd = tokio::process::Command::new(&self.binary);
        cmd.current_dir(&self.workspace_root)
            .arg("run")
            .args(argv)
            .stdin(Stdio::inherit())
            .stdout(Stdio::inherit())
            .stderr(Stdio::inherit())
            .kill_on_drop(true);
        cmd.envs(&spec.env);
        let mut child = cmd.spawn()?;
        let status = child.wait().await?;
        let bep = read_bep_or_none(&bep_file)?;
        Ok(ExecResult { code: status.code().unwrap_or(-1), bep: Some(bep) })
    }

    /// `bazel test` with BEP capture and junit XML parsing.
    pub async fn exec_test(&self, pv: &ProjectView, spec: &ExecSpec) -> Result<ExecResult, ExecError> {
        self.exec_with_bep("test", pv, spec, |argv| {
            argv.extend(pv.test_flags.iter().cloned());
            if let Some(filter) = &spec.test_filter {
                argv.push(format!("--test_filter={filter}"));
            }
            argv.push("--test_output=errors".to_string());
            Ok(())
        })
        .await
    }

    /// `bazel coverage` producing lcov reports referenced by BEP.
    pub async fn exec_coverage(&self, pv: &ProjectView, spec: &ExecSpec) -> Result<ExecResult, ExecError> {
        self.exec_with_bep("coverage", pv, spec, |argv| {
            argv.extend(pv.build_flags.iter().cloned());
            argv.push("--combined_report=lcov".to_string());
            Ok(())
        })
        .await
    }

    /// Shared path: build argv, run with inherited console + BEP sidecar,
    /// parse the sidecar after exit.
    async fn exec_with_bep(
        &self,
        command: &str,
        _pv: &ProjectView,
        spec: &ExecSpec,
        extra_flags: impl FnOnce(&mut Vec<String>) -> Result<(), BazelError>,
    ) -> Result<ExecResult, ExecError> {
        if spec.labels.is_empty() {
            return Err(BazelError::Other("no targets given".into()).into());
        }
        let bep_file = temp_bep_file();
        let mut argv: Vec<String> = vec![];
        common_flags(&mut argv);
        // `test` output groups don't exist; skip build_tag_filters for coverage
        if command == "coverage" {
            argv.retain(|a| a != BUILD_TAG_FILTERS);
        }
        extra_flags(&mut argv)?;
        bep_flags(&bep_file, &mut argv);
        argv.extend(spec.labels.iter().cloned());
        argv.extend(spec.extra_args.iter().cloned());

        let mut cmd = tokio::process::Command::new(&self.binary);
        cmd.current_dir(&self.workspace_root)
            .arg(command) // startup options come after the command, we have none
            .args(argv)
            .stdin(Stdio::null())
            .stdout(Stdio::inherit())
            .stderr(Stdio::inherit())
            .kill_on_drop(true);
        cmd.envs(&spec.env);
        let status = cmd.spawn()?.wait().await?;

        let bep = read_bep_or_none(&bep_file)?;
        let code = status.code().unwrap_or(-1);
        Ok(ExecResult { code, bep: Some(bep) })
    }
}

fn temp_bep_file() -> std::path::PathBuf {
    std::env::temp_dir()
        .join(format!("baztet-bep-{}-{}.bin", std::process::id(), chrono_id()))
}

fn chrono_id() -> u128 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0)
}

fn read_bep_or_none(path: &std::path::Path) -> Result<BepOutput, ExecError> {
    match std::fs::read(path) {
        Ok(bytes) => {
            let _ = std::fs::remove_file(path);
            Ok(baztet_bep::parse(&bytes)?)
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(BepOutput::default()),
        Err(e) => Err(e.into()),
    }
}

/// One test case from a junit `test.xml`.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct TestCase {
    pub class_name: String,
    pub name: String,
    pub status: TestCaseStatus,
    pub duration_seconds: Option<f64>,
    /// failure/expectation message, if any
    pub message: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub enum TestCaseStatus {
    Passed,
    Failed,
    Skipped,
    Error,
}

/// Parse a junit-produced `test.xml` (as written by bazel's
/// `--test_output` XML file) into cases.
pub fn parse_test_xml(bytes: &[u8]) -> Vec<TestCase> {
    let text = String::from_utf8_lossy(bytes);
    let mut reader = quick_xml::Reader::from_str(&text);
    reader.config_mut().trim_text(true);
    let mut cases: Vec<TestCase> = Vec::new();
    let mut in_case = false;
    let mut current = TestCase {
        class_name: String::new(),
        name: String::new(),
        status: TestCaseStatus::Passed,
        duration_seconds: None,
        message: None,
    };
    let mut current_failure: Option<String> = None;
    use quick_xml::events::Event;
    loop {
        match reader.read_event() {
            Ok(Event::Start(e)) if e.name().as_ref() == b"testcase" => {
                in_case = true;
                current = TestCase {
                    class_name: String::new(),
                    name: String::new(),
                    status: TestCaseStatus::Passed,
                    duration_seconds: None,
                    message: None,
                };
                current_failure = None;
                for attr in e.attributes().flatten() {
                    match attr.key.as_ref() {
                        b"classname" => current.class_name = attr.unescape_value().unwrap_or_default().into_owned(),
                        b"name" => current.name = attr.unescape_value().unwrap_or_default().into_owned(),
                        b"time" => current.duration_seconds = attr.unescape_value().unwrap_or_default().parse().ok(),
                        _ => {}
                    }
                }
            }
            Ok(Event::Start(e)) if in_case && (e.name().as_ref() == b"failure" || e.name().as_ref() == b"error") => {
                current.status = if e.name().as_ref() == b"failure" {
                    TestCaseStatus::Failed
                } else {
                    TestCaseStatus::Error
                };
                if let Some(attr) = e.attributes().flatten().find(|a| a.key.as_ref() == b"message") {
                    current_failure = Some(attr.unescape_value().unwrap_or_default().into_owned());
                }
            }
            Ok(Event::Empty(e)) if in_case && (e.name().as_ref() == b"failure" || e.name().as_ref() == b"error") => {
                current.status = if e.name().as_ref() == b"failure" {
                    TestCaseStatus::Failed
                } else {
                    TestCaseStatus::Error
                };
                if let Some(attr) = e.attributes().flatten().find(|a| a.key.as_ref() == b"message") {
                    current_failure = Some(attr.unescape_value().unwrap_or_default().into_owned());
                }
            }
            Ok(Event::Empty(e)) if in_case && e.name().as_ref() == b"skipped" => {
                current.status = TestCaseStatus::Skipped;
            }
            Ok(Event::Empty(e)) if e.name().as_ref() == b"testcase" => {
                // self-closing testcase: passed, attributes only
                let mut case = TestCase {
                    class_name: String::new(),
                    name: String::new(),
                    status: TestCaseStatus::Passed,
                    duration_seconds: None,
                    message: None,
                };
                for attr in e.attributes().flatten() {
                    match attr.key.as_ref() {
                        b"classname" => case.class_name = attr.unescape_value().unwrap_or_default().into_owned(),
                        b"name" => case.name = attr.unescape_value().unwrap_or_default().into_owned(),
                        b"time" => case.duration_seconds = attr.unescape_value().unwrap_or_default().parse().ok(),
                        _ => {}
                    }
                }
                cases.push(case);
            }
            Ok(Event::End(e)) if e.name().as_ref() == b"testcase" => {
                current.message = current_failure.take();
                cases.push(current.clone());
                in_case = false;
            }
            Ok(Event::Eof) => break,
            Err(e) => {
                eprintln!("[baztet] junit xml truncated/unparsable near {}: {e}", reader.buffer_position());
                break;
            }
            _ => {}
        }
    }
    cases
}

/// One IntelliJ Bazel run configuration recipe (`.bazel/*.xml`,
/// `BlazeCommandRunConfigurationType`): the CLI equivalent of the IDE's
/// run configurations, imported via the project view.
#[derive(Debug, Clone, Default, PartialEq, serde::Serialize)]
pub struct Recipe {
    pub name: String,
    /// bazel command: test | run | build
    pub command: String,
    pub target: String,
    pub env: BTreeMap<String, String>,
}

/// Parse a run-configuration XML; only the Blaze fields are consumed.
pub fn parse_recipe(xml: &str) -> Option<Recipe> {
    use quick_xml::events::Event;

    let mut reader = quick_xml::Reader::from_str(xml);
    reader.config_mut().trim_text(true);
    let mut recipe = Recipe::default();
    let mut in_blaze = false;
    let mut in_envs = false;
    loop {
        let ev = reader.read_event().ok()?;
        match ev {
            Event::Start(e) => match e.name().as_ref() {
                b"configuration" => {
                    if let Some(attr) = e.attributes().flatten().find(|a| a.key.as_ref() == b"name") {
                        recipe.name = attr.unescape_value().unwrap_or_default().into_owned();
                    }
                }
                b"blaze-settings" => {
                    in_blaze = true;
                    if let Some(attr) = e.attributes().flatten().find(|a| a.key.as_ref() == b"blaze-command") {
                        recipe.command = attr.unescape_value().unwrap_or_default().into_owned();
                    }
                }
                b"env_state" => in_envs = true,
                b"env" if in_envs => {
                    let (name, value) = env_entry(&e);
                    recipe.env.insert(name, value);
                }
                _ => {}
            },
            Event::Empty(e) => {
                if in_envs && e.name().as_ref() == b"env" {
                    let (name, value) = env_entry(&e);
                    recipe.env.insert(name, value);
                }
            }
            Event::Text(t) => {
                if in_blaze && !in_envs {
                    let text = t.unescape().unwrap_or_default().to_string();
                    if text.contains("//") {
                        recipe.target = text;
                    }
                }
            }
            Event::Eof => break,
            _ => {}
        }
    }
    if recipe.target.is_empty() || recipe.command.is_empty() {
        return None;
    }
    Some(recipe)
}

fn env_entry(e: &quick_xml::events::BytesStart) -> (String, String) {
    let mut name = String::new();
    let mut value = String::new();
    for attr in e.attributes().flatten() {
        match attr.key.as_ref() {
            b"name" => name = attr.unescape_value().unwrap_or_default().into_owned(),
            b"value" => value = attr.unescape_value().unwrap_or_default().into_owned(),
            _ => {}
        }
    }
    (name, value)
}

/// Resolve test.xml paths from BEP test outputs against the execution root
/// and parse them into case lists.
pub fn test_cases_from_bep(
    runner: &BazelRunner,
    bep: &BepOutput,
) -> Vec<(String /* label */, Vec<TestCase>)> {
    let execution_root = &runner.info.execution_root;
    let mut out: Vec<(String, Vec<TestCase>)> = Vec::new();
    for (label, cases) in group_by_label(bep) {
        let mut all: Vec<TestCase> = Vec::new();
        for path in cases {
            let full = execution_root.join(&path);
            if let Ok(bytes) = std::fs::read(full) {
                all.extend(parse_test_xml(&bytes));
            }
        }
        out.push((label, all));
    }
    out
}

fn group_by_label(bep: &BepOutput) -> Vec<(String, Vec<String>)> {
    // flaky tests re-run: only the LAST attempt's xml reflects the final
    // result; attempt 1 failures would otherwise be reported as active
    let mut out: Vec<(String, Vec<String>)> = Vec::new();
    for summary in bep.test_results.iter().rev() {
        if out.iter().any(|(l, _)| *l == summary.label) {
            continue;
        }
        if let Some(xml) = &summary.xml {
            out.push((summary.label.clone(), vec![xml.relative_path()]));
        }
    }
    out.reverse();
    out
}


#[cfg(test)]
mod tests {
    use super::*;

    const JUNIT: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<testsuite name="com.x.A" tests="3" failures="1" errors="0">
  <testcase name="okCase" classname="com.x.A" time="0.05"/>
  <testcase name="badCase" classname="com.x.A" time="0.10">
    <failure message="expected 42">stack...</failure>
  </testcase>
  <testcase name="skippedCase" classname="com.x.A">
    <skipped/>
  </testcase>
</testsuite>
"#;

    #[test]
    fn junit_parse() {
        let cases = parse_test_xml(JUNIT.as_bytes());
        assert_eq!(cases.len(), 3);
        assert_eq!(cases[0].status, TestCaseStatus::Passed);
        assert_eq!(cases[0].duration_seconds, Some(0.05));
        assert_eq!(cases[1].status, TestCaseStatus::Failed);
        assert_eq!(cases[1].message.as_deref(), Some("expected 42"));
        assert_eq!(cases[2].status, TestCaseStatus::Skipped);
        assert_eq!(cases[0].class_name, "com.x.A");
    }

    #[test]
    fn recipe_parse_full() {
        let xml = r#"<configuration name="api unit tests" type="BlazeCommandRunConfigurationType" factoryName="Bazel Command">
  <blaze-settings handler-id="BlazeCommandGenericRunConfigurationHandlerProvider" kind="java_test" blaze-command="test">
    <blaze-target>//pkg/api:unit</blaze-target>
    <env_state>
      <envs>
        <env name="CONFIG_PATH" value="config.yaml"/>
      </envs>
    </env_state>
  </blaze-settings>
</configuration>"#;
        let r = parse_recipe(xml).expect("parses");
        assert_eq!(r.name, "api unit tests");
        assert_eq!(r.command, "test");
        assert_eq!(r.target, "//pkg/api:unit");
        assert_eq!(r.env.get("CONFIG_PATH").map(String::as_str), Some("config.yaml"));
    }

    #[test]
    fn recipe_rejects_incomplete() {
        assert!(parse_recipe(r#"<configuration><blaze-settings></blaze-settings></configuration>"#).is_none());
        // target without command
        assert!(parse_recipe(
            r#"<configuration><blaze-settings blaze-command=""><blaze-target>//a:b</blaze-target></blaze-settings></configuration>"#
        )
        .is_none());
    }

    #[test]
    fn last_attempt_wins_in_reports() {
        use baztet_bep::{BepFile, TestResultDetail};
        let bep = BepOutput {
            test_results: vec![
                // flaky rerun: attempt 1 failed, attempt 2 passed
                TestResultDetail {
                    label: "//a:t".into(),
                    status: "FAILED".into(),
                    cached_locally: false,
                    xml: Some(BepFile { path_prefix: vec![], name: "a/first/test.xml".into() }),
                    log: None,
                },
                TestResultDetail {
                    label: "//a:t".into(),
                    status: "PASSED".into(),
                    cached_locally: false,
                    xml: Some(BepFile { path_prefix: vec![], name: "a/second/test.xml".into() }),
                    log: None,
                },
            ],
            ..Default::default()
        };
        let groups = group_by_label(&bep);
        // only the LAST attempt's xml is reported
        assert_eq!(groups, [("//a:t".to_string(), vec!["a/second/test.xml".to_string()])]);
    }

    #[test]
    fn junit_error_case() {
        let xml = r#"<testsuite><testcase name="e"><error message="boom"/></testcase></testsuite>"#;
        let cases = parse_test_xml(xml.as_bytes());
        assert_eq!(cases[0].status, TestCaseStatus::Error);
        assert_eq!(cases[0].message.as_deref(), Some("boom"));
    }

}