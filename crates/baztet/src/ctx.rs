//! Resolve workspace/PV/runner/snapshot from CLI options — the context every
//! command needs, built once.

use camino::Utf8PathBuf;
use std::process::ExitCode;

use baztet_core::exec::ExecSpec;
use baztet_core::BazelRunner;

use crate::Cli;

/// Bootstrap for exec commands: runner + workspace + pv (+ snapshot when
/// asked). Sync, doctor and recipes build their context by hand because
/// their pv/refresh rules differ.
pub struct Ctx {
    pub runner: BazelRunner,
    pub pv: Option<baztet_projectview::ProjectView>,
    pub snapshot: Option<baztet_snapshot::Snapshot>,
}

impl Ctx {
    pub async fn exec(cli: &Cli, refresh: bool, need_snapshot: bool) -> anyhow::Result<Self> {
        let runner = runner_for(cli, refresh).await?;
        let ws = workspace_of(cli)?;
        let snapshot = if need_snapshot {
            Some(load_snapshot(ws.clone())?)
        } else {
            None
        };
        let pv = Some(load_pv(&runner, cli.project_view.as_ref())?);
        Ok(Self {
            runner,
            pv,
            snapshot,
        })
    }

    /// Snapshot or fail with the sync hint.
    pub fn snapshot(&self) -> anyhow::Result<&baztet_snapshot::Snapshot> {
        self.snapshot
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("no snapshot loaded"))
    }

    /// The parsed project view (always present in exec commands).
    pub fn pv(&self) -> &baztet_projectview::ProjectView {
        self.pv.as_ref().unwrap()
    }
}

pub fn exit_code(code: i32) -> ExitCode {
    u8::try_from(code).map_or(ExitCode::FAILURE, ExitCode::from)
}

/// Report a bazel command failure on stderr and carry its exit code.
pub fn exit_for(command: &str, code: i32) -> ExitCode {
    if code != 0 {
        eprintln!("[baztet] {command} failed with exit code {code}");
    }
    exit_code(code)
}

pub fn spec(labels: Vec<String>, passthrough: Vec<String>) -> ExecSpec {
    ExecSpec {
        labels,
        extra_args: passthrough,
        ..Default::default()
    }
}

pub fn test_labels(
    snapshot: &baztet_snapshot::Snapshot,
    labels: &[String],
    pattern: &Option<String>,
) -> anyhow::Result<Vec<String>> {
    if !labels.is_empty() {
        return Ok(labels.to_vec());
    }
    let tests: Vec<String> = snapshot
        .graph
        .iter()
        .filter(|t| t.kind.ends_with("_test") || t.kind.ends_with("test_suite"))
        .map(|t| t.label.clone())
        .collect();
    if tests.is_empty() {
        anyhow::bail!("no *_test targets in the snapshot; run `baztet sync` first");
    }
    if let Some(p) = pattern {
        let selected: Vec<String> = tests
            .iter()
            .filter(|l| l.contains(p.as_str()))
            .cloned()
            .collect();
        if selected.is_empty() {
            anyhow::bail!("no test target matches '{p}'");
        }
        return Ok(selected);
    }
    Ok(tests)
}

pub fn default_labels(
    snapshot: &baztet_snapshot::Snapshot,
    labels: &[String],
) -> anyhow::Result<Vec<String>> {
    if !labels.is_empty() {
        return Ok(labels.to_vec());
    }
    if snapshot.specified_targets.is_empty() {
        anyhow::bail!("no targets to build; run `baztet sync` first");
    }
    Ok(snapshot.specified_targets.clone())
}

pub fn workspace_of(cli: &Cli) -> anyhow::Result<Utf8PathBuf> {
    match &cli.workspace {
        Some(ws) => Ok(ws.clone()),
        None => {
            let cwd = Utf8PathBuf::try_from(std::env::current_dir()?)?;
            baztet_core::workspace::find_workspace_root(&cwd)
                .ok_or_else(|| anyhow::anyhow!("no bazel workspace above {cwd}"))
        }
    }
}

pub fn load_snapshot(ws_root: Utf8PathBuf) -> anyhow::Result<baztet_snapshot::Snapshot> {
    baztet_snapshot::Snapshot::load(&ws_root)
        .map_err(|e| anyhow::anyhow!("no snapshot under {ws_root}: {e} — run `baztet sync` first"))
}

/// Parse a project view for binary resolution; None when it is missing or
/// unparseable (a generated BSP view with unresolvable imports must not
/// break every command; full parse errors surface where the PV matters).
pub fn parse_pv_soft(
    path: &Utf8PathBuf,
    ws: &Utf8PathBuf,
) -> Option<baztet_projectview::ProjectView> {
    match baztet_projectview::parse(path, ws) {
        Ok(pv) => Some(pv),
        Err(e) => {
            eprintln!("[baztet] project view at {path} unparseable ({e}); using defaults");
            None
        }
    }
}

pub fn load_pv(
    runner: &BazelRunner,
    explicit: Option<&Utf8PathBuf>,
) -> anyhow::Result<baztet_projectview::ProjectView> {
    let path = explicit.cloned().unwrap_or_else(|| {
        baztet_core::workspace::find_project_view(&runner.workspace_root)
            .unwrap_or_else(|| runner.workspace_root.join(".bazelproject"))
    });
    if !path.is_file() {
        if explicit.is_some() {
            anyhow::bail!("project view not found at {path}");
        }
        eprintln!("[baztet] no project view at {path}; running with default flags");
        return Ok(Default::default());
    }
    // discovered views degrade to defaults when unparseable; an explicit
    // --project-view must fail loudly instead
    if explicit.is_none() {
        return Ok(parse_pv_soft(&path, &runner.workspace_root).unwrap_or_default());
    }
    Ok(baztet_projectview::parse(&path, &runner.workspace_root)?)
}

/// Runner plus tuning flags (`--no-output-check`) and quiet mode.
pub async fn runner_for(cli: &Cli, refresh: bool) -> anyhow::Result<BazelRunner> {
    let mut runner = discover(cli, refresh).await?;
    if cli.no_output_check {
        runner
            .tuning_flags
            .push("--noexperimental_check_output_files".to_string());
    }
    runner.quiet = !cli.raw;
    Ok(runner)
}

/// Discover workspace + bazel binary; without `refresh`, reuse the
/// snapshot's cached info text so exec commands skip the info spawn
/// (only when the snapshot's bazel_binary matches).
pub async fn discover(cli: &Cli, refresh: bool) -> anyhow::Result<BazelRunner> {
    let ws = workspace_of(cli)?;
    let binary = resolve_binary(cli, &ws);
    if !refresh {
        if let Ok(snapshot) = baztet_snapshot::Snapshot::load(&ws) {
            // reuse the cached info only when the snapshot came from the
            // same binary: a --bazel swap must not keep the old
            // execution_root (paths would resolve wrong silently)
            let cached = snapshot
                .bazel_info_text
                .as_ref()
                .filter(|_| snapshot.bazel_binary.as_deref() == Some(binary.as_str()));
            if let Some(text) = cached {
                let info = baztet_core::workspace::parse_info(text);
                return Ok(BazelRunner::from_cached_info(&binary, ws, info));
            }
        }
    }
    Ok(BazelRunner::discover(&binary, &ws).await?)
}

/// The bazel binary every command must agree on: `--bazel`, else the
/// project view's `bazel_binary`, else `bazel`. A PV parse failure falls
/// back instead of blocking the command.
pub fn resolve_binary(cli: &Cli, ws: &Utf8PathBuf) -> String {
    match &cli.bazel {
        Some(b) => b.clone(),
        None => baztet_core::workspace::find_project_view(ws)
            .and_then(|p| parse_pv_soft(&p, ws).map(|pv| pv.bazel_binary().to_owned()))
            .unwrap_or_else(|| "bazel".to_string()),
    }
}
