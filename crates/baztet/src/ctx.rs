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

#[cfg(test)]
mod tests {
    use super::*;
    use baztet_snapshot::Snapshot;
    use camino::Utf8PathBuf;
    use clap::Parser;

    #[test]
    fn exit_code_maps_into_u8() {
        assert_eq!(exit_code(0), ExitCode::SUCCESS);
        assert_eq!(exit_code(3), ExitCode::from(3u8));
        assert_eq!(exit_code(-1), ExitCode::FAILURE);
        assert_eq!(exit_code(300), ExitCode::FAILURE);
    }

    #[test]
    fn spec_builds_exec_spec() {
        let s = spec(vec!["//a:b".into()], vec!["--x".into()]);
        assert_eq!(s.labels, ["//a:b"]);
        assert_eq!(s.extra_args, ["--x"]);
        assert!(s.test_filter.is_none());
    }

    #[test]
    fn label_helpers_pick_snapshot_defaults() {
        let mut snap = Snapshot::new("/ws".into());
        snap.specified_targets = vec!["//a:all".into()];
        assert_eq!(
            default_labels(&snap, &[]).unwrap(),
            vec!["//a:all".to_string()]
        );
        assert_eq!(
            default_labels(&snap, &["//b".into()]).unwrap(),
            ["//b".to_string()]
        );
        // empty explicit labels and empty snapshot -> helpful error
        let empty = Snapshot::new("/ws".into());
        assert!(default_labels(&empty, &[]).is_err());
    }

    #[test]
    fn test_labels_direct_pattern_and_miss() {
        let mut snap = Snapshot::new("/ws".into());
        let _ = snap
            .graph
            .get_or_insert("//a:unit_test", "sh_test", None, vec![]);
        let _ = snap
            .graph
            .get_or_insert("//a:lib", "java_library", None, vec![]);
        // explicit labels win
        assert_eq!(
            test_labels(&snap, &["//x:y".into()], &None).unwrap(),
            ["//x:y".to_string()]
        );
        // default: only *_test targets
        let defaults = test_labels(&snap, &[], &None).unwrap();
        assert_eq!(defaults, ["//a:unit_test".to_string()]);
        // pattern matches subset
        assert_eq!(
            test_labels(&snap, &[], &Some("unit".into())).unwrap(),
            ["//a:unit_test".to_string()]
        );
        // pattern with no match
        assert!(test_labels(&snap, &[], &Some("nope".into())).is_err());
        // no tests in snapshot
        let mut snap2 = Snapshot::new("/ws".into());
        let _ = snap2
            .graph
            .get_or_insert("//a:lib", "java_library", None, vec![]);
        assert!(test_labels(&snap2, &[], &None).is_err());
    }

    #[test]
    fn parse_pv_soft_degrades_on_bad_file() {
        let dir = std::env::temp_dir().join(format!("baztet-pvsoft-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let bad = Utf8PathBuf::from_path_buf(dir.join("bad.bazelproject")).unwrap();
        std::fs::write(&bad, "targets:\n  - not a section\nunknown_section: x\n").unwrap();
        let ws = Utf8PathBuf::from_path_buf(dir.clone()).unwrap();
        // unknown section errors -> soft None
        assert!(parse_pv_soft(&bad, &ws).is_none());
        // a valid empty view parses
        let good = Utf8PathBuf::from_path_buf(dir.join("good.bazelproject")).unwrap();
        std::fs::write(&good, "build_flags:\n").unwrap();
        assert!(parse_pv_soft(&good, &ws).is_some());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn resolve_binary_chain() {
        let pv = Cli::parse_from(["baztet", "--bazel", "/custom/bazel", "doctor"]);
        let ws = Utf8PathBuf::from("/ws");
        assert_eq!(resolve_binary(&pv, &ws), "/custom/bazel");
        // no --bazel, no PV -> plain bazel
        let cli = Cli::parse_from(["baztet", "doctor"]);
        assert_eq!(
            resolve_binary(&cli, &Utf8PathBuf::from("/nonexistent-ws")),
            "bazel"
        );
    }

    #[test]
    fn workspace_of_cli_and_default() {
        // explicit --workspace wins
        let cli = Cli::parse_from(["baztet", "--workspace", "/tmp/nowhere-baztet", "doctor"]);
        assert_eq!(workspace_of(&cli).unwrap().as_str(), "/tmp/nowhere-baztet");
        // no flag: nearest workspace above CWD. Only asserted when the test
        // process itself runs inside a workspace (the repo root has none,
        // and CWD cannot be changed safely under parallel tests)
        if let Ok(found) = workspace_of(&Cli::parse_from(["baztet", "doctor"])) {
            assert!(baztet_core::workspace::find_workspace_root(&found).is_some());
        }
    }
}
