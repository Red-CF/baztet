//! baztet CLI: standalone sync/execution tool for Bazel projects.

use std::collections::HashSet;
use std::process::ExitCode;

use baztet_core::{sync as sync_engine, BazelError, BazelRunner};
use camino::Utf8PathBuf;
use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(name = "baztet", version, about = "Standalone Bazel project sync and execution tool")]
struct Cli {
    /// path to the bazel binary (project view `bazel_binary` overrides)
    #[arg(long, global = true, env = "BAZTET_BAZEL")]
    bazel: Option<String>,

    /// workspace directory (defaults to the closest one above cwd)
    #[arg(long, global = true)]
    workspace: Option<Utf8PathBuf>,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Sync the project: query phase + aspect phase, save the snapshot.
    Sync {
        /// project view file (defaults to <workspace>/.bazelproject)
        #[arg(long)]
        project_view: Option<Utf8PathBuf>,
        /// print the resulting snapshot as JSON to stdout
        #[arg(long)]
        json: bool,
        /// force a full re-query even when the project view is unchanged
        #[arg(long)]
        fresh: bool,
        /// stop after the fast query phase (skip aspect analysis)
        #[arg(long)]
        fast: bool,
    },
    /// Run `bazel build` on labels (or every synced target).
    #[command(after_help = "Anything after `--` is passed through to bazel.")]
    Build {
        labels: Vec<String>,
        /// args after `--` go straight to bazel
        #[arg(last = true)]
        passthrough: Vec<String>,
    },
    /// Run `bazel test` with BEP-based results and junit XML case reports.
    #[command(after_help = "Anything after `--` is passed through to bazel.")]
    Test {
        /// labels; defaults to every *_test target in the snapshot
        labels: Vec<String>,
        /// value for `--test_filter`
        #[arg(long)]
        filter: Option<String>,
        /// select test targets by label substring instead of listing all
        #[arg(long, short)]
        pattern: Option<String>,
        /// print per-case results from test.xml files
        #[arg(long)]
        cases: bool,
        /// print a machine-readable report to stdout
        #[arg(long)]
        json: bool,
        #[arg(last = true)]
        passthrough: Vec<String>,
    },
    /// Run one target with `bazel run` (output streamed).
    #[command(after_help = "Anything after `--` is passed through to bazel.")]
    Run {
        label: String,
        #[arg(last = true)]
        passthrough: Vec<String>,
    },
    /// Run `bazel coverage` and point at the merged lcov report.
    Coverage {
        labels: Vec<String>,
        #[arg(last = true)]
        passthrough: Vec<String>,
    },
    /// Check the environment: bazel binary/version, workspace, project view,
    /// snapshot, aspect deployment.
    Doctor,
    /// Benchmarks: startup, proto stream decode, snapshot roundtrip.
    Bench {
        /// number of synthetic targets to stream-decode
        #[arg(long, default_value = "50000")]
        targets: u64,
    },
    /// List imported run configurations (recipes) from the project view.
    Recipes {
        /// project view file (defaults to <workspace>/.bazelproject)
        #[arg(long)]
        project_view: Option<Utf8PathBuf>,
        /// run one recipe: `baztet test --recipe acceptance-tests`
        #[arg(long, short)]
        run: Option<String>,
        /// print a machine-readable report to stdout
        #[arg(long)]
        json: bool,
    },
    /// List targets in the synced snapshot.
    Targets {
        /// label substring, or rule kind with '=' prefix (e.g. =java_library)
        #[arg(long, short)]
        filter: Option<String>,
        #[arg(long)]
        json: bool,
    },
    /// Show dependencies of a target.
    Deps {
        /// target label like //foo:bar
        label: String,
        /// show reverse dependencies instead
        #[arg(long)]
        reverse: bool,
        /// include transitive dependencies
        #[arg(long, short = 'r')]
        transitive: bool,
    },
}

#[tokio::main(flavor = "multi_thread")]
async fn main() -> ExitCode {
    let cli = Cli::parse();
    match run(&cli).await {
        Ok(code) => code,
        Err(e) => {
            eprintln!("[baztet] error: {e:#}");
            for cause in e.chain() {
                if let Some(BazelError::Failure(_, code, _)) = cause.downcast_ref() {
                    return exit_code(*code);
                }
            }
            ExitCode::FAILURE
        }
    }
}

fn exit_code(code: i32) -> ExitCode {
    u8::try_from(code).map_or(ExitCode::FAILURE, ExitCode::from)
}

async fn run(cli: &Cli) -> anyhow::Result<ExitCode> {
    use baztet_core::exec::{ExecSpec, Recipe};

    fn spec(labels: Vec<String>, passthrough: Vec<String>) -> ExecSpec {
        ExecSpec { labels, extra_args: passthrough, ..Default::default() }
    }

    fn exit_for(command: &str, code: i32) -> ExitCode {
        if code != 0 {
            eprintln!("[baztet] {command} failed with exit code {code}");
        }
        exit_code(code)
    }

    /// Test labels: explicit, else tests matching `pattern`, else all tests.
    fn test_labels(
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
            let selected: Vec<String> =
                tests.iter().filter(|l| l.contains(p.as_str())).cloned().collect();
            if selected.is_empty() {
                anyhow::bail!("no test target matches '{p}'");
            }
            return Ok(selected);
        }
        Ok(tests)
    }

    match &cli.command {
        Command::Sync { project_view, json, fresh, fast } => {
            let runner = discover(cli).await?;
            let pv_path =
                project_view.clone().unwrap_or_else(|| runner.workspace_root.join(".bazelproject"));
            if !pv_path.is_file() {
                anyhow::bail!("project view not found at {pv_path}");
            }
            let pv = baztet_projectview::parse(&pv_path, &runner.workspace_root)?;
            let snapshot = sync_engine::sync(&runner, &pv, !fast, *fresh).await?;
            let path = snapshot.save()?;
            eprintln!(
                "[baztet] synced {} target(s), {} -> {path}",
                snapshot.graph.len(),
                if snapshot.has_error { "with errors" } else { "ok" },
            );
            if *json {
                println!("{}", snapshot.to_json(true)?);
            }
            Ok(ExitCode::SUCCESS)
        }
        Command::Build { labels, passthrough } => {
            let runner = discover(cli).await?;
            let snapshot = load_snapshot(workspace_of(cli)?)?;
            let labels = default_labels(&snapshot, labels)?;
            let result = runner.exec_build(&load_pv(&runner, None)?, &spec(labels.clone(), passthrough.clone())).await?;
            Ok(exit_for("build", result.code))
        }
        Command::Test { labels, filter, pattern, cases, json, passthrough } => {
            let runner = discover(cli).await?;
            let snapshot = load_snapshot(workspace_of(cli)?)?;
            let labels = test_labels(&snapshot, labels, pattern)?;
            let mut spec = spec(labels, passthrough.clone());
            spec.test_filter = filter.clone();
            let result = runner.exec_test(&load_pv(&runner, None)?, &spec).await?;

            let bep = result.bep.as_ref().ok_or_else(|| anyhow::anyhow!("no BEP output"))?;
            let per_target: Vec<(&String, &String)> =
                bep.test_summaries.iter().map(|s| (&s.label, &s.status)).collect();
            let parsed = baztet_core::exec::test_cases_from_bep(&runner, bep);
            let failed = result.code != 0
                || bep.test_summaries.iter().any(|s| s.status != "PASSED");

            if *json {
                let report = serde_json::json!({
                    "exit_code": result.code,
                    "summaries": bep.test_summaries,
                    "cases": parsed.iter().map(|(label, cases)| {
                        serde_json::json!({
                            "label": label,
                            "cases": cases,
                        })
                    }).collect::<Vec<_>>(),
                });
                println!("{}", serde_json::to_string_pretty(&report)?);
            } else {
                for (label, status) in &per_target {
                    println!("{status:>8}  {label}");
                }
                if *cases {
                    for (label, case_list) in &parsed {
                        for case in case_list {
                            if case.status != baztet_core::exec::TestCaseStatus::Passed {
                                println!("    {case:?}  ({label})");
                            }
                        }
                    }
                }
            }
            Ok(exit_for("test", if failed && result.code == 0 { 3 } else { result.code }))
        }
        Command::Run { label, passthrough } => {
            let runner = discover(cli).await?;
            let result =
                runner.exec_run(&load_pv(&runner, None)?, &spec(vec![label.clone()], passthrough.clone())).await?;
            Ok(exit_for("run", result.code))
        }
        Command::Coverage { labels, passthrough } => {
            let runner = discover(cli).await?;
            let snapshot = load_snapshot(workspace_of(cli)?)?;
            let labels = default_labels(&snapshot, labels)?;
            let result = runner.exec_coverage(&load_pv(&runner, None)?, &spec(labels.clone(), passthrough.clone())).await?;
            if let Some(bep) = result.bep.as_ref() {
                // lcov merged report referenced in BEP build tool logs / outputs
                let merged: Vec<_> = bep
                    .named_sets
                    .values()
                    .flat_map(|s| &s.files)
                    .filter(|f| f.name.contains("lcov"))
                    .map(|f| f.relative_path())
                    .collect();
                for path in merged {
                    println!("{path}");
                }
            }
            Ok(exit_for("coverage", result.code))
        }
        Command::Recipes { project_view, run, json } => {
            let runner = discover(cli).await?;
            let pv = load_pv(&runner, project_view.as_ref())?;
            let mut recipes: Vec<Recipe> = Vec::new();
            let mut missing = 0usize;
            for path in &pv.import_run_configurations {
                let rel = path.trim_start_matches('/');
                match std::fs::read_to_string(runner.workspace_root.join(rel)) {
                    Ok(xml) => {
                        if let Some(r) = baztet_core::exec::parse_recipe(&xml) {
                            recipes.push(r);
                        }
                    }
                    Err(_) => missing += 1,
                }
            }
            if *json {
                println!("{}", serde_json::to_string_pretty(&recipes)?);
            } else {
                if missing > 0 {
                    eprintln!("[baztet] {missing} recipe(s) not found on disk");
                }
                for r in &recipes {
                    println!("{}: {} {}  ({} env)", r.name, r.command, r.target, r.env.len());
                }
            }
            match run {
                Some(name) => {
                    let Some(recipe) = recipes.iter().find(|r| {
                        r.name.contains(name.as_str())
                            || name.is_empty()
                            || r.target.contains(name.as_str())
                    }) else {
                        anyhow::bail!("recipe '{name}' not found; see `baztet recipes`");
                    };
                    eprintln!("[baztet] recipe '{}': {} {}", recipe.name, recipe.command, recipe.target);
                    let mut spec = ExecSpec {
                        labels: vec![recipe.target.clone()],
                        env: recipe.env.clone(),
                        ..Default::default()
                    };
                    if recipe.command == "test" {
                        // acceptance/unit recipes run their target; no extra filter
                        let result = runner.exec_test(&pv, &spec).await?;
                        Ok(exit_for("test", result.code))
                    } else if recipe.command == "build" {
                        let result = runner.exec_build(&pv, &spec).await?;
                        Ok(exit_for("build", result.code))
                    } else if recipe.command == "run" {
                        spec.extra_args.push("--single_top_level_target".into());
                        let result = runner.exec_run(&pv, &spec).await?;
                        Ok(exit_for("run", result.code))
                    } else {
                        anyhow::bail!("unsupported recipe command: {}", recipe.command);
                    }
                }
                None => Ok(ExitCode::SUCCESS),
            }
        }
        Command::Doctor => {
            use std::fmt::Write as DocWrite;
            let mut report = String::new();
            let mut ok = true;

            writeln!(report, "baztet doctor")?;
            // bazel
            let binary = cli.bazel.as_deref().unwrap_or("bazel");
            let version = std::process::Command::new(binary).arg("--version").output();
            match version {
                Ok(out) if out.status.success() => {
                    writeln!(report, "bazel: {} ({})", binary, String::from_utf8_lossy(&out.stdout).trim())?;
                }
                _ => {
                    writeln!(report, "bazel: {binary} NOT FOUND or not runnable")?;
                    ok = false;
                }
            }
            // workspace
            let ws = workspace_of(cli)?;
            writeln!(report, "workspace: {ws}")?;
            // project view
            let pv_path = ws.join(".bazelproject");
            match pv_path.is_file() {
                true => writeln!(report, "project view: {pv_path}")?,
                false => writeln!(report, "project view: not present (scoped views elsewhere)")?,
            }
            // snapshot
            match baztet_snapshot::Snapshot::load(&ws) {
                Ok(snapshot) => writeln!(
                    report,
                    "snapshot: {} target(s), {} ide entries{}, last fingerprint {:?}",
                    snapshot.graph.len(),
                    snapshot.ide_infos.len(),
                    if snapshot.has_error { " (has errors)" } else { "" },
                    snapshot.sync_fingerprint,
                )?,
                Err(e) => writeln!(report, "snapshot: none ({e})")?,
            }
            // aspect dir
            let aspect = ws.join(baztet_core::sync::ASPECT_PREFIX);
            match aspect.join("config/aspect.bzl").is_file() {
                true => writeln!(report, "aspect: deployed at {aspect}")?,
                false => writeln!(report, "aspect: not deployed yet (first sync) and .bazelbsp (IDE) state unknown; not an error")?,
            }
            print!("{report}");
            Ok(if ok { ExitCode::SUCCESS } else { exit_code(1) })
        }
        Command::Bench { targets } => {
            let result = baztet_core::bench::run(*targets).await?;
            eprintln!(
                "[baztet] decoded {} targets in {:?}; 10k-target snapshot roundtrip {:?}",
                result.decoded, result.decode_duration, result.snapshot_roundtrip
            );
            Ok(ExitCode::SUCCESS)
        }
        Command::Targets { filter, json } => {
            let snapshot = load_snapshot(workspace_of(cli)?)?;
            let mut targets: Vec<&baztet_snapshot::TargetNode> = snapshot
                .graph
                .iter()
                .filter(|t| match filter {
                    Some(f) => {
                        if t.label.contains(f.as_str()) {
                            return true;
                        }
                        f.strip_prefix('=').is_some_and(|k| t.kind == k)
                    }
                    None => true,
                })
                .collect();
            targets.sort_by(|a, b| a.label.cmp(&b.label));
            if *json {
                println!("{}", serde_json::to_string_pretty(&targets)?);
            } else {
                for t in targets {
                    println!("{} [{}]", t.label, t.kind);
                }
            }
            Ok(ExitCode::SUCCESS)
        }
        Command::Deps { label, reverse, transitive } => {
            if !label.starts_with("//") && !label.starts_with('@') {
                anyhow::bail!("not a bazel label: {label}");
            }
            let snapshot = load_snapshot(workspace_of(cli)?)?;
            let Some(idx) = snapshot.graph.index_of(label) else {
                anyhow::bail!("target {label} not in snapshot; run `baztet sync` first");
            };
            let graph = &snapshot.graph;
            let labels: Vec<String> = if *reverse {
                let mut out = vec![];
                let mut seen = HashSet::new();
                let mut queue = vec![idx];
                let mut pos = 0;
                if *transitive {
                    seen.insert(idx);
                }
                while pos < queue.len() {
                    let node_idx = queue[pos];
                    pos += 1;
                    for dep in graph.reverse_deps(node_idx) {
                        let dep_idx = graph.index_of(&dep.label).unwrap();
                        if seen.insert(dep_idx) {
                            out.push(dep.label.clone());
                            queue.push(dep_idx);
                        }
                    }
                }
                out.sort();
                out
            } else {
                let direct = &graph.get_by_index(idx).unwrap().deps;
                let indices = if *transitive { graph.transitive_deps(idx) } else { direct.clone() };
                indices
                    .into_iter()
                    .skip(if *transitive { 1 } else { 0 })
                    .filter_map(|i| graph.get_by_index(i).map(|t| t.label.clone()))
                    .collect()
            };
            for l in &labels {
                println!("{l}");
            }
            Ok(ExitCode::SUCCESS)
        }
    }
}

fn default_labels(snapshot: &baztet_snapshot::Snapshot, labels: &[String]) -> anyhow::Result<Vec<String>> {
    if !labels.is_empty() {
        return Ok(labels.to_vec());
    }
    if snapshot.specified_targets.is_empty() {
        anyhow::bail!("no targets to build; run `baztet sync` first");
    }
    Ok(snapshot.specified_targets.clone())
}

fn load_pv(runner: &BazelRunner, explicit: Option<&Utf8PathBuf>) -> anyhow::Result<baztet_projectview::ProjectView> {
    let path = explicit
        .cloned()
        .unwrap_or_else(|| runner.workspace_root.join(".bazelproject"));
    if !path.is_file() {
        if explicit.is_some() {
            anyhow::bail!("project view not found at {path}");
        }
        eprintln!("[baztet] no project view at {path}; running with default flags");
        return Ok(Default::default());
    }
    Ok(baztet_projectview::parse(&path, &runner.workspace_root)?)
}

/// Project view for exec commands: defaults to `<ws>/.bazelproject`, falls
/// back to an empty view when the workspace has none (scoped project views
/// live wherever the user keeps them).
fn workspace_of(cli: &Cli) -> anyhow::Result<Utf8PathBuf> {
    match &cli.workspace {
        Some(ws) => Ok(ws.clone()),
        None => {
            let cwd = Utf8PathBuf::try_from(std::env::current_dir()?)?;
            baztet_core::workspace::find_workspace_root(&cwd)
                .ok_or_else(|| anyhow::anyhow!("no bazel workspace above {cwd}"))
        }
    }
}

fn load_snapshot(ws_root: Utf8PathBuf) -> anyhow::Result<baztet_snapshot::Snapshot> {
    baztet_snapshot::Snapshot::load(&ws_root)
        .map_err(|e| anyhow::anyhow!("no snapshot under {ws_root}: {e} — run `baztet sync` first"))
}

/// Discover workspace + probe the bazel binary.
async fn discover(cli: &Cli) -> anyhow::Result<BazelRunner> {
    let ws = workspace_of(cli)?;
    let binary = cli.bazel.as_deref().unwrap_or("bazel");
    Ok(BazelRunner::discover(binary, &ws).await?)
}