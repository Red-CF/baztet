//! Execution engine: spawn build/test/run/coverage bazel commands with the
//! ExecuteService-contract flag construction, BEP sidecars, and bounded
//! console capture. Presentation lives in the CLI (spinners, dashboards);
//! sync keeps its own single parse-progress bar for its long internal phase.

use std::collections::BTreeMap;
use std::process::Stdio;

use baztet_bep::{BepError, BepOutput};
use thiserror::Error;


use crate::bazel::{BazelError, BazelRunner};
use crate::capture::drain;

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

// junit parsing and recipes moved out; kept here for the callers
pub use crate::capture::ConsoleCapture;
pub use crate::junit::{parse_test_xml, test_cases_from_bep, TestCase, TestCaseStatus};
pub use crate::recipe::{parse_recipe, Recipe};

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
        pv: &baztet_projectview::ProjectView,
        spec: &ExecSpec,
    ) -> Result<ExecResult, ExecError> {
        let child = self.exec_build_fork(pv, spec, self.quiet).await?;
        child.wait().await
    }

    /// `bazel build` forked without waiting: the dashboard polls live console.
    pub async fn exec_build_fork(
        &self,
        pv: &baztet_projectview::ProjectView,
        spec: &ExecSpec,
        capture: bool,
    ) -> Result<ExecChild, ExecError> {
        self.exec_fork("build", pv, spec, capture, |argv| {
            argv.extend(pv.build_flags.iter().cloned());
            Ok(())
        })
        .await
    }

    pub async fn exec_run(
        &self,
        pv: &baztet_projectview::ProjectView,
        spec: &ExecSpec,
    ) -> Result<ExecResult, ExecError> {
        if spec.labels.len() != 1 {
            return Err(BazelError::Other("`run` takes exactly one target".into()).into());
        }
        let mut argv: Vec<String> = vec![];
        common_flags(&mut argv, false);
        argv.extend(pv.build_flags.iter().cloned());
        let bep_file = temp_bep_file();
        // BEP + tuning flags BEFORE the label: postpositional args go to
        // the binary, so bazel-side flags must never land after it
        bep_flags(&bep_file, &mut argv);
        argv.extend(self.tuning_flags.iter().cloned());
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
        Ok(ExecResult {
            code: status.code().unwrap_or(-1),
            bep: Some(bep),
            ..Default::default()
        })
    }

    /// `bazel test` with BEP capture and junit XML parsing.
    pub async fn exec_test(
        &self,
        pv: &baztet_projectview::ProjectView,
        spec: &ExecSpec,
    ) -> Result<ExecResult, ExecError> {
        let child = self.exec_test_fork(pv, spec, self.quiet).await?;
        child.wait().await
    }

    /// `bazel test` forked without waiting: the TUI polls the live BEP.
    pub async fn exec_test_fork(
        &self,
        pv: &baztet_projectview::ProjectView,
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
        pv: &baztet_projectview::ProjectView,
        spec: &ExecSpec,
    ) -> Result<ExecResult, ExecError> {
        let child = self.exec_coverage_fork(pv, spec, self.quiet).await?;
        child.wait().await
    }

    /// `bazel coverage` forked without waiting: the dashboard polls live.
    pub async fn exec_coverage_fork(
        &self,
        pv: &baztet_projectview::ProjectView,
        spec: &ExecSpec,
        capture: bool,
    ) -> Result<ExecChild, ExecError> {
        self.exec_fork("coverage", pv, spec, capture, |argv| {
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
        _pv: &baztet_projectview::ProjectView,
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
        let console = ConsoleCapture::new();
        let (out_task, err_task) =
            if let (Some(out), Some(err)) = (child.stdout.take(), child.stderr.take()) {
                (
                    Some(tokio::spawn(drain(out, console.clone()))),
                    Some(tokio::spawn(drain(err, console.clone()))),
                )
            } else {
                (None, None)
            };

        Ok(ExecChild {
            command: command.to_string(),
            child: Some(child),
            bep_file,
            captured: capture,
            console,
            out_task,
            err_task,
            started,
        })
    }
}

/// One spawned bazel exec command: call [`ExecChild::wait`] for the
/// result, or poll [`ExecChild::bep_path`] while it runs (live TUI).
pub struct ExecChild {
    command: String,
    child: Option<tokio::process::Child>,
    bep_file: std::path::PathBuf,
    captured: bool,
    /// live console, written by the drain tasks as bazel runs
    console: ConsoleCapture,
    out_task: Option<tokio::task::JoinHandle<()>>,
    err_task: Option<tokio::task::JoinHandle<()>>,
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

    /// Handle to the live console for dashboard progress; clone it before
    /// moving `self` into the waiting task.
    pub fn console_handle(&self) -> ConsoleCapture {
        self.console.clone()
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
        if let Some(t) = self.out_task.take() {
            t.await.map_err(join_err)?;
        }
        if let Some(t) = self.err_task.take() {
            t.await.map_err(join_err)?;
        }
        let bep = read_bep_or_none(&self.bep_file)?;
        Ok(ExecResult {
            code: status.code().unwrap_or(-1),
            secs: self.started.elapsed().as_secs_f64(),
            output: self.captured.then(|| {
                let text = self.console.text();
                // the two streams interleaved into one tail; presentation
                // works on lines, warnings and failure dump alike
                ExecCapture {
                    stdout: text.into_bytes(),
                    stderr: Vec::new(),
                }
            }),
            bep: Some(bep),
        })
    }
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
}