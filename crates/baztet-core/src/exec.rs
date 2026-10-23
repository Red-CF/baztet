//! Execution: build/test/run/coverage with ExecuteService-contract flag construction, BEP sidecars beside live console output, and junit test.xml parsing.

use std::collections::BTreeMap;
use std::io::IsTerminal;
use std::process::Stdio;

use baztet_bep::{BepError, BepOutput};
use tokio::io::AsyncReadExt;

/// bazel junit test.xml files are XML 1.0; quick-xml 0.41 normalizes
/// attribute/text values per XML version.
const XML_VERSION: quick_xml::XmlVersion = quick_xml::XmlVersion::Implicit1_0;
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
    /// bazel's raw console output, captured only in quiet mode
    /// (`BazelRunner::quiet` or the TUI) so the CLI can post-process it.
    pub output: Option<ExecCapture>,
    /// wall-clock time of the bazel command
    pub secs: f64,
}

/// Captured bazel stdout/stderr; present only when output was captured.
#[derive(Debug, Clone, Default)]
pub struct ExecCapture {
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
}

/// Warning lines extracted from a captured run, deduped, stdout first.
/// O(matches) memory beyond one lossy copy of each stream.
pub fn warn_lines(capture: &ExecCapture) -> Vec<String> {
    let mut seen = std::collections::HashSet::new();
    let mut out = Vec::new();
    for stream in [&capture.stdout, &capture.stderr] {
        let text = String::from_utf8_lossy(stream);
        for line in text.lines() {
            if !line.trim().is_empty()
                && line.to_ascii_lowercase().contains("warning")
                && seen.insert(line.to_string())
            {
                out.push(line.to_string());
            }
        }
    }
    out
}

pub const BUILD_TAG_FILTERS: &str = "--build_tag_filters=-no-ide";

fn common_flags(argv: &mut Vec<String>, plain: bool) {
    argv.push("--tool_tag=baztet".to_string());
    // captured runs post-process the output, so keep it color-free instead
    // of pushing both flags and relying on last-occurrence-wins
    argv.push(if plain { "--color=no" } else { "--color=yes" }.to_string());
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
    pub async fn exec_build(
        &self,
        pv: &ProjectView,
        spec: &ExecSpec,
    ) -> Result<ExecResult, ExecError> {
        let child = self.exec_build_fork(pv, spec).await?;
        child.wait_with_ui(self.quiet).await
    }

    /// `bazel build` forked without waiting: the dashboard polls the live BEP.
    pub async fn exec_build_fork(
        &self,
        pv: &ProjectView,
        spec: &ExecSpec,
    ) -> Result<ExecChild, ExecError> {
        self.exec_fork("build", pv, spec, true, |argv| {
            argv.extend(pv.build_flags.iter().cloned());
            Ok(())
        })
        .await
    }

    pub async fn exec_run(
        &self,
        pv: &ProjectView,
        spec: &ExecSpec,
    ) -> Result<ExecResult, ExecError> {
        if spec.labels.len() != 1 {
            return Err(BazelError::Other("`run` takes exactly one target".into()).into());
        }
        let mut argv: Vec<String> = vec![];
        common_flags(&mut argv, false);
        argv.extend(pv.build_flags.iter().cloned());
        let bep_file = temp_bep_file();
        // BEP flags BEFORE the label: postpositional args go to the binary
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
        if !self.tuning_flags.is_empty() {
            cmd.args(&self.tuning_flags);
        }
        let mut child = cmd.spawn()?;
        let status = child.wait().await?;
        let bep = read_bep_or_none(&bep_file)?;
        Ok(ExecResult {
            code: status.code().unwrap_or(-1),
            bep: Some(bep),
            ..Default::default()
        })
    }

    /// `bazel test` with BEP capture and junit XML parsing.
    pub async fn exec_test(
        &self,
        pv: &ProjectView,
        spec: &ExecSpec,
    ) -> Result<ExecResult, ExecError> {
        let child = self.exec_test_fork(pv, spec, self.quiet).await?;
        child.wait_with_ui(self.quiet).await
    }

    /// `bazel test` forked without waiting: the TUI polls the live BEP.
    pub async fn exec_test_fork(
        &self,
        pv: &ProjectView,
        spec: &ExecSpec,
        capture: bool,
    ) -> Result<ExecChild, ExecError> {
        self.exec_fork("test", pv, spec, capture, |argv| {
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
    pub async fn exec_coverage(
        &self,
        pv: &ProjectView,
        spec: &ExecSpec,
    ) -> Result<ExecResult, ExecError> {
        let child = self.exec_coverage_fork(pv, spec).await?;
        child.wait_with_ui(self.quiet).await
    }

    /// `bazel coverage` forked without waiting: the dashboard polls live.
    pub async fn exec_coverage_fork(
        &self,
        pv: &ProjectView,
        spec: &ExecSpec,
    ) -> Result<ExecChild, ExecError> {
        self.exec_fork("coverage", pv, spec, true, |argv| {
            argv.extend(pv.build_flags.iter().cloned());
            argv.push("--combined_report=lcov".to_string());
            Ok(())
        })
        .await
    }

    /// Spawn one exec command; the caller decides how to wait on it
    /// (plain [`ExecChild::wait`], live BEP polling for the dashboard, ...).
    pub async fn exec_fork(
        &self,
        command: &str,
        _pv: &ProjectView,
        spec: &ExecSpec,
        capture: bool,
        extra_flags: impl FnOnce(&mut Vec<String>) -> Result<(), BazelError>,
    ) -> Result<ExecChild, ExecError> {
        if spec.labels.is_empty() {
            return Err(BazelError::Other("no targets given".into()).into());
        }
        let bep_file = temp_bep_file();
        let mut argv: Vec<String> = vec![];
        // captured output is post-processed, so keep it color-free and the
        // failure dump readable
        common_flags(&mut argv, capture);
        if command == "coverage" {
            argv.retain(|a| *a != BUILD_TAG_FILTERS);
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
            .kill_on_drop(true);
        if capture {
            cmd.stdout(Stdio::piped()).stderr(Stdio::piped());
        } else {
            cmd.stdout(Stdio::inherit()).stderr(Stdio::inherit());
        }
        cmd.envs(&spec.env);
        if !self.tuning_flags.is_empty() {
            cmd.args(&self.tuning_flags);
        }
        let started = std::time::Instant::now();
        let mut child = cmd.spawn()?;

        // drain the pipes while bazel runs: a 64 KiB OS buffer fills on any
        // build of real size and waiting on both processes deadlocks
        let (out_task, err_task) =
            if let (Some(out), Some(err)) = (child.stdout.take(), child.stderr.take()) {
                (
                    Some(tokio::spawn(drain(out))),
                    Some(tokio::spawn(drain(err))),
                )
            } else {
                (None, None)
            };

        Ok(ExecChild {
            command: command.to_string(),
            child: Some(child),
            bep_file,
            captured: capture,
            out_task,
            err_task,
            started,
        })
    }
}

/// drain one piped stream, keeping a bounded tail (the buffer sits
/// between `DRAIN_CAP` and `2×DRAIN_CAP` between trims): presentation
/// shows warnings + a failure tail, and buffering a pathological build's
/// full console output would OOM the CLI
async fn drain<S: tokio::io::AsyncRead + Unpin>(mut stream: S) -> Vec<u8> {
    const DRAIN_CAP: usize = 256 * 1024;
    let mut buf = Vec::new();
    let mut chunk = [0u8; 8192];
    loop {
        match stream.read(&mut chunk).await {
            Ok(0) | Err(_) => break,
            Ok(n) => {
                buf.extend_from_slice(&chunk[..n]);
                if buf.len() > DRAIN_CAP * 2 {
                    buf.drain(..buf.len() - DRAIN_CAP);
                }
            }
        }
    }
    buf
}

/// One spawned bazel exec command: call [`ExecChild::wait`] for the
/// result, or poll [`ExecChild::bep_path`] while it runs (live TUI).
pub struct ExecChild {
    command: String,
    child: Option<tokio::process::Child>,
    bep_file: std::path::PathBuf,
    captured: bool,
    out_task: Option<tokio::task::JoinHandle<Vec<u8>>>,
    err_task: Option<tokio::task::JoinHandle<Vec<u8>>>,
    started: std::time::Instant,
}

impl ExecChild {
    /// Path bazel streams the Build Event Protocol to while running;
    /// incomplete at any instant, so re-parse failures are normal.
    pub fn bep_path(&self) -> &std::path::Path {
        &self.bep_file
    }

    pub fn command(&self) -> &str {
        &self.command
    }

    /// Parse the BEP sidecar as bazel writes it. None when the file is
    /// absent or mid-write: a truncated tail fails to parse, and the
    /// caller keeps its previous snapshot.
    pub fn live_bep_snapshot(path: &std::path::Path) -> Option<BepOutput> {
        let bytes = std::fs::read(path).ok()?;
        baztet_bep::parse(&bytes).ok()
    }

    /// Final blocking wait that consumes the spawned command and returns
    /// the full result. The TUI's live-completion check is instead
    /// `JoinHandle::is_finished` while the waiter task holds this.
    pub async fn wait(mut self) -> Result<ExecResult, ExecError> {
        let Some(mut child) = self.child.take() else {
            return Err(BazelError::Other("child already taken".into()).into());
        };
        let status = child.wait().await?;
        // a panicked drain task must not surface as silent empty output
        let join_err =
            |e: tokio::task::JoinError| BazelError::Other(format!("console capture failed: {e}"));
        let stdout = match self.out_task.take() {
            Some(t) => t.await.map_err(join_err)?,
            None => Vec::new(),
        };
        let stderr = match self.err_task.take() {
            Some(t) => t.await.map_err(join_err)?,
            None => Vec::new(),
        };
        let bep = read_bep_or_none(&self.bep_file)?;
        Ok(ExecResult {
            code: status.code().unwrap_or(-1),
            secs: self.started.elapsed().as_secs_f64(),
            output: self.captured.then_some(ExecCapture { stdout, stderr }),
            bep: Some(bep),
        })
    }

    /// Wait with the Bastet spinner when the output is captured on a live
    /// terminal; plain wait otherwise.
    pub async fn wait_with_ui(self, ui: bool) -> Result<ExecResult, ExecError> {
        if !ui || !std::io::stderr().is_terminal() {
            return self.wait().await;
        }
        let bar = cat_spinner(&self.command);
        let result = self.wait().await;
        bar.finish_and_clear();
        result
    }
}

/// `baztet build` — a cat mid-pounce; steady tick drives it while bazel runs.
fn cat_spinner(command: &str) -> indicatif::ProgressBar {
    const FRAMES: &[&str] = &[" ( ^ω^ )ฅ", " (=^ω^=) ", " ₍^ω^₎ ", "ฅ( ^ω^ ) "];
    let bar = indicatif::ProgressBar::new_spinner();
    bar.enable_steady_tick(std::time::Duration::from_millis(120));
    bar.set_style(
        indicatif::ProgressStyle::with_template("{spinner} {msg} · {elapsed}")
            .expect("static spinner template")
            .tick_strings(FRAMES),
    );
    bar.set_message(format!("baztet {command}"));
    bar
}

fn temp_bep_file() -> std::path::PathBuf {
    std::env::temp_dir().join(format!(
        "baztet-bep-{}-{}.bin",
        std::process::id(),
        chrono_id()
    ))
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

/// Parse a junit `test.xml` into cases.
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
            Ok(Event::Start(e)) if e.name().as_ref() == "testcase" => {
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
                        "classname" => {
                            current.class_name = attr
                                .normalized_value(XML_VERSION)
                                .unwrap_or_default()
                                .into_owned()
                        }
                        "name" => {
                            current.name = attr
                                .normalized_value(XML_VERSION)
                                .unwrap_or_default()
                                .into_owned()
                        }
                        "time" => {
                            current.duration_seconds = attr
                                .normalized_value(XML_VERSION)
                                .unwrap_or_default()
                                .parse()
                                .ok()
                        }
                        _ => {}
                    }
                }
            }
            Ok(Event::Start(e))
                if in_case && (e.name().as_ref() == "failure" || e.name().as_ref() == "error") =>
            {
                current.status = if e.name().as_ref() == "failure" {
                    TestCaseStatus::Failed
                } else {
                    TestCaseStatus::Error
                };
                if let Some(attr) = e
                    .attributes()
                    .flatten()
                    .find(|a| a.key.as_ref() == "message")
                {
                    current_failure = Some(
                        attr.normalized_value(XML_VERSION)
                            .unwrap_or_default()
                            .into_owned(),
                    );
                }
            }
            Ok(Event::Empty(e))
                if in_case && (e.name().as_ref() == "failure" || e.name().as_ref() == "error") =>
            {
                current.status = if e.name().as_ref() == "failure" {
                    TestCaseStatus::Failed
                } else {
                    TestCaseStatus::Error
                };
                if let Some(attr) = e
                    .attributes()
                    .flatten()
                    .find(|a| a.key.as_ref() == "message")
                {
                    current_failure = Some(
                        attr.normalized_value(XML_VERSION)
                            .unwrap_or_default()
                            .into_owned(),
                    );
                }
            }
            Ok(Event::Empty(e)) if in_case && e.name().as_ref() == "skipped" => {
                current.status = TestCaseStatus::Skipped;
            }
            Ok(Event::Empty(e)) if e.name().as_ref() == "testcase" => {
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
                        "classname" => {
                            case.class_name = attr
                                .normalized_value(XML_VERSION)
                                .unwrap_or_default()
                                .into_owned()
                        }
                        "name" => {
                            case.name = attr
                                .normalized_value(XML_VERSION)
                                .unwrap_or_default()
                                .into_owned()
                        }
                        "time" => {
                            case.duration_seconds = attr
                                .normalized_value(XML_VERSION)
                                .unwrap_or_default()
                                .parse()
                                .ok()
                        }
                        _ => {}
                    }
                }
                cases.push(case);
            }
            Ok(Event::End(e)) if e.name().as_ref() == "testcase" => {
                current.message = current_failure.take();
                cases.push(current.clone());
                in_case = false;
            }
            Ok(Event::Eof) => break,
            Err(e) => {
                eprintln!(
                    "[baztet] junit xml truncated/unparsable near {}: {e}",
                    reader.buffer_position()
                );
                break;
            }
            _ => {}
        }
    }
    cases
}

/// IntelliJ Bazel run config (`.bazel/*.xml`) as a CLI recipe.
#[derive(Debug, Clone, Default, PartialEq, serde::Serialize)]
pub struct Recipe {
    pub name: String,
    /// bazel command: test | run | build
    pub command: String,
    pub target: String,
    pub env: BTreeMap<String, String>,
}

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
                "configuration" => {
                    if let Some(attr) = e.attributes().flatten().find(|a| a.key.as_ref() == "name")
                    {
                        recipe.name = attr
                            .normalized_value(XML_VERSION)
                            .unwrap_or_default()
                            .into_owned();
                    }
                }
                "blaze-settings" => {
                    in_blaze = true;
                    if let Some(attr) = e
                        .attributes()
                        .flatten()
                        .find(|a| a.key.as_ref() == "blaze-command")
                    {
                        recipe.command = attr
                            .normalized_value(XML_VERSION)
                            .unwrap_or_default()
                            .into_owned();
                    }
                }
                "env_state" => in_envs = true,
                "env" if in_envs => {
                    let (name, value) = env_entry(&e);
                    recipe.env.insert(name, value);
                }
                _ => {}
            },
            Event::Empty(e) => {
                if in_envs && e.name().as_ref() == "env" {
                    let (name, value) = env_entry(&e);
                    recipe.env.insert(name, value);
                }
            }
            Event::Text(t) => {
                if in_blaze && !in_envs {
                    let text = t.xml_content(XML_VERSION).into_owned();
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
            "name" => {
                name = attr
                    .normalized_value(XML_VERSION)
                    .unwrap_or_default()
                    .into_owned()
            }
            "value" => {
                value = attr
                    .normalized_value(XML_VERSION)
                    .unwrap_or_default()
                    .into_owned()
            }
            _ => {}
        }
    }
    (name, value)
}

/// junit cases per test label, resolved against the execution root.
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

    #[test]
    fn warn_lines_dedupe_and_match_both_streams() {
        let capture = ExecCapture {
            stdout: b"Compiling...\nWARNING: deprecation in ::a\nCompiling B\n".to_vec(),
            stderr: b"warning: unused variable\nWARNING: deprecation in ::a\n".to_vec(),
        };
        let warns = warn_lines(&capture);
        assert_eq!(
            warns,
            ["WARNING: deprecation in ::a", "warning: unused variable"]
        );
    }

    #[test]
    fn warn_lines_rejects_noise_and_blank() {
        let capture = ExecCapture {
            stdout: b"https://x.com/progress\n   \nwarning: only\n".to_vec(),
            stderr: Vec::new(),
        };
        assert_eq!(warn_lines(&capture), ["warning: only"]);
    }

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
        assert_eq!(
            r.env.get("CONFIG_PATH").map(String::as_str),
            Some("config.yaml")
        );
    }

    #[test]
    fn recipe_rejects_incomplete() {
        assert!(parse_recipe(
            r#"<configuration><blaze-settings></blaze-settings></configuration>"#
        )
        .is_none());
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
                    xml: Some(BepFile {
                        path_prefix: vec![],
                        name: "a/first/test.xml".into(),
                    }),
                    log: None,
                },
                TestResultDetail {
                    label: "//a:t".into(),
                    status: "PASSED".into(),
                    cached_locally: false,
                    xml: Some(BepFile {
                        path_prefix: vec![],
                        name: "a/second/test.xml".into(),
                    }),
                    log: None,
                },
            ],
            ..Default::default()
        };
        let groups = group_by_label(&bep);
        // only the LAST attempt's xml is reported
        assert_eq!(
            groups,
            [("//a:t".to_string(), vec!["a/second/test.xml".to_string()])]
        );
    }

    #[test]
    fn junit_error_case() {
        let xml = r#"<testsuite><testcase name="e"><error message="boom"/></testcase></testsuite>"#;
        let cases = parse_test_xml(xml.as_bytes());
        assert_eq!(cases[0].status, TestCaseStatus::Error);
        assert_eq!(cases[0].message.as_deref(), Some("boom"));
    }
}
