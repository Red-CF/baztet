use std::collections::HashSet;
use std::io::IsTerminal;
use std::process::ExitCode;

#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

use baztet_core::{sync as sync_engine, BazelError, BazelRunner};
use camino::Utf8PathBuf;
use clap::{Parser, Subcommand};

mod tui;

// stderr presentation; --json and machine outputs stay untouched

use owo_colors::OwoColorize;
use owo_colors::Stream;

fn ok(msg: &str) {
    eprintln!(
        "{}",
        format!("✓ {msg}").if_supports_color(Stream::Stderr, |t| t.green().to_string())
    );
}

fn warn(msg: &str) {
    eprintln!(
        "{}",
        format!("! {msg}").if_supports_color(Stream::Stderr, |t| t.yellow().to_string())
    );
}

fn fail(msg: &str) {
    eprintln!(
        "{}",
        format!("✗ {msg}").if_supports_color(Stream::Stderr, |t| t.red().to_string())
    );
}

fn phase(n: u8, total: u8, label: &str, secs: f64) {
    eprintln!(
        "{}",
        format!("[{n}/{total}] {label} ({secs:.1}s)")
            .if_supports_color(Stream::Stderr, |t| t.blue().to_string())
    );
}

fn test_status_colored(status: &str) -> String {
    // pad the plain status first: width formatting on an ANSI-wrapped string
    // pads against escape bytes, not visible width; style after padding and
    // gate on the stream support
    format!("{status:>8}")
        .if_supports_color(Stream::Stdout, |t| match status {
            "PASSED" => t.bright_green().to_string(),
            "FAILED" => t.bright_red().to_string(),
            _ => t.bright_yellow().to_string(),
        })
        .to_string()
}

/// Does this BEP status fail the exit code? Any non-PASSED verdict does
/// (CI gates on the exit code); display buckets are finer than this.
fn status_is_failure(status: &str) -> bool {
    status != "PASSED"
}

#[derive(Parser)]
#[command(
    name = "baztet",
    version,
    about = "Standalone Bazel project sync and execution tool"
)]
struct Cli {
    /// path to the bazel binary (project view `bazel_binary` overrides)
    #[arg(long, global = true, env = "BAZTET_BAZEL")]
    bazel: Option<String>,

    /// workspace directory (defaults to the closest one above cwd)
    #[arg(long, global = true)]
    workspace: Option<Utf8PathBuf>,

    /// project view file (defaults to <workspace>/.bazelproject)
    #[arg(long, global = true)]
    project_view: Option<Utf8PathBuf>,

    /// snapshot cache directory (defaults to the platform cache dir;
    /// env: BAZTET_CACHE_DIR)
    #[arg(long, global = true, env = "BAZTET_CACHE_DIR")]
    cache_dir: Option<Utf8PathBuf>,

    /// skip bazel's output-file freshness scan (`--noexperimental_check_
    /// output_files`): warm syncs get faster, stale outputs are trusted
    #[arg(long, global = true)]
    no_output_check: bool,

    /// stream bazel's raw console output instead of the quiet summary
    /// (quiet mode captures it, shows warnings, and dumps errors on failure)
    #[arg(long, global = true)]
    raw: bool,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Sync the project: query phase + aspect phase, save the snapshot.
    Sync {
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
        /// live per-target dashboard over the running tests (alt screen)
        #[arg(long)]
        tui: bool,
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
    /// Passthrough: any unrecognized `baztet <cmd> [args...]` runs bazel
    /// directly in the discovered workspace (drop-in replacement).
    #[command(external_subcommand)]
    Bazel(Vec<String>),
}

fn main() -> ExitCode {
    // parse + env write before any runtime spawns worker threads:
    // process env is not thread-safe, and BAZTET_CACHE_DIR is read later
    let cli = Cli::parse();
    if let Some(dir) = &cli.cache_dir {
        std::env::set_var("BAZTET_CACHE_DIR", dir);
    }
    async_main(&cli)
}

#[tokio::main(flavor = "multi_thread")]
async fn async_main(cli: &Cli) -> ExitCode {
    match run(cli).await {
        Ok(code) => code,
        Err(e) => {
            fail(&format!("{e}"));
            for cause in e.chain().skip(1) {
                eprintln!("  ↳ {cause}");
            }
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

/// passthrough: `baztet <anything-not-a-baztet-command>` = bazel with the
/// same args, in the discovered workspace, streamed. --raw-ish by design.
async fn exec_bazel_passthrough(cli: &Cli, args: Vec<String>) -> anyhow::Result<ExitCode> {
    let ws = workspace_of(cli)?;
    // same binary chain every other command uses; a PV parse failure does
    // not block the escape hatch
    let binary = match &cli.bazel {
        Some(b) => b.clone(),
        None => baztet_core::workspace::find_project_view(&ws)
            .and_then(|p| parse_pv_soft(&p, &ws).map(|pv| pv.bazel_binary().to_owned()))
            .unwrap_or_else(|| "bazel".to_string()),
    };
    eprintln!(
        "[baztet] '{}' is not a baztet command; running bazel directly",
        args.first().map(String::as_str).unwrap_or("")
    );
    let mut cmd = tokio::process::Command::new(&binary)
        .current_dir(&ws)
        .args(args)
        .kill_on_drop(true)
        .spawn()?;
    let status = cmd.wait().await?;
    Ok(exit_code(status.code().unwrap_or(-1)))
}

/// Parse a project view for binary resolution; None when it is missing or
/// unparseable (a generated BSP view with unresolvable imports must not
/// break every command; full parse errors surface where the PV matters).
fn parse_pv_soft(path: &Utf8PathBuf, ws: &Utf8PathBuf) -> Option<baztet_projectview::ProjectView> {
    match baztet_projectview::parse(path, ws) {
        Ok(pv) => Some(pv),
        Err(e) => {
            eprintln!("[baztet] project view at {path} unparseable ({e}); using defaults");
            None
        }
    }
}

async fn run(cli: &Cli) -> anyhow::Result<ExitCode> {
    use baztet_core::exec::{ExecSpec, Recipe};

    fn spec(labels: Vec<String>, passthrough: Vec<String>) -> ExecSpec {
        ExecSpec {
            labels,
            extra_args: passthrough,
            ..Default::default()
        }
    }

    fn exit_for(command: &str, code: i32) -> ExitCode {
        if code != 0 {
            eprintln!("[baztet] {command} failed with exit code {code}");
        }
        exit_code(code)
    }

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

    match &cli.command {
        Command::Sync { json, fresh, fast } => {
            let runner = runner_for(cli, true).await?;
            let pv_path = match &cli.project_view {
                Some(p) => {
                    if !p.is_file() {
                        anyhow::bail!("project view not found at {p}");
                    }
                    p.clone()
                }
                None => baztet_core::workspace::find_project_view(&runner.workspace_root)
                    .ok_or_else(|| {
                        anyhow::anyhow!(
                            "no project view; tried {}. create one, or pass --project-view",
                            baztet_core::workspace::PV_CANDIDATES.join(", ")
                        )
                    })?,
            };
            let pv = baztet_projectview::parse(&pv_path, &runner.workspace_root)?;
            let report = sync_engine::sync(&runner, &pv, !fast, *fresh).await?;
            let snapshot = &report.snapshot;
            let path = snapshot.save()?;

            match report.reuse {
                sync_engine::Reuse::Everything => warn(&format!(
                    "nothing changed: snapshot reused ({} target(s), {} ide entries)",
                    snapshot.graph.len(),
                    snapshot.ide_infos.len()
                )),
                sync_engine::Reuse::Graph => {
                    phase(
                        1,
                        2,
                        "query graph reused",
                        report.query_secs.unwrap_or_default(),
                    );
                    phase(
                        2,
                        2,
                        "aspect outputs",
                        report.aspect_secs.unwrap_or_default(),
                    );
                }
                sync_engine::Reuse::FullQuery => {
                    phase(1, 2, "query", report.query_secs.unwrap_or_default());
                    // the aspect phase prints only when it ran (--fast skips it)
                    if let Some(secs) = report.aspect_secs {
                        phase(2, 2, "aspect build + BEP", secs);
                    }
                }
            }

            let line = format!(
                "synced {} target(s) · {} ide entries · snapshot {path}",
                snapshot.graph.len(),
                snapshot.ide_infos.len()
            );
            if snapshot.has_error {
                fail(&format!("{line} (some outputs failed)"));
            } else {
                ok(&line);
            }
            if *json {
                println!("{}", snapshot.to_json(true)?);
            }
            Ok(ExitCode::SUCCESS)
        }
        Command::Build {
            labels,
            passthrough,
        } => {
            let runner = runner_for(cli, false).await?;
            let snapshot = load_snapshot(workspace_of(cli)?)?;
            let labels = default_labels(&snapshot, labels)?;
            let result = runner
                .exec_build(
                    &load_pv(&runner, None)?,
                    &spec(labels.clone(), passthrough.clone()),
                )
                .await?;
            present_exec("build", &result, true);
            Ok(exit_for("build", result.code))
        }
        Command::Test {
            labels,
            filter,
            pattern,
            cases,
            json,
            tui,
            passthrough,
        } => {
            let runner = runner_for(cli, false).await?;
            let snapshot = load_snapshot(workspace_of(cli)?)?;
            let labels = test_labels(&snapshot, labels, pattern)?;
            let mut spec = spec(labels.clone(), passthrough.clone());
            spec.test_filter = filter.clone();
            let result = match (*tui, std::io::stdout().is_terminal()) {
                (true, true) => {
                    let child = runner
                        .exec_test_fork(&load_pv(&runner, None)?, &spec, true)
                        .await?;
                    tui::run_test(child, labels.clone()).await?
                }
                // --tui without a tty (pipe, CI): fall back to the quiet capture
                (true, false) => {
                    eprintln!("[baztet] --tui needs a terminal; running quietly");
                    runner.exec_test(&load_pv(&runner, None)?, &spec).await?
                }
                (false, _) => runner.exec_test(&load_pv(&runner, None)?, &spec).await?,
            };
            present_exec("test", &result, false);

            let bep = result
                .bep
                .as_ref()
                .ok_or_else(|| anyhow::anyhow!("no BEP output"))?;
            let per_target: Vec<(&String, &String)> = bep
                .test_summaries
                .iter()
                .map(|s| (&s.label, &s.status))
                .collect();
            let parsed = baztet_core::exec::test_cases_from_bep(&runner, bep);
            let failed = result.code != 0
                || bep
                    .test_summaries
                    .iter()
                    .any(|s| status_is_failure(&s.status));

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
                let (mut pass, mut fail, mut other) = (0usize, 0usize, 0usize);
                for (label, status) in &per_target {
                    println!("{}  {label}", test_status_colored(status));
                    match status.as_str() {
                        "PASSED" => pass += 1,
                        // test verdicts that count against the exit code;
                        // anything else (CANCELLED, NO_STATUS, ...) is
                        // neither passed nor failed by a verdict
                        "FAILED" | "TIMEOUT" | "FLAKY" => fail += 1,
                        _ => other += 1,
                    }
                }
                if *cases {
                    use baztet_core::exec::TestCaseStatus;
                    for (label, case_list) in &parsed {
                        for case in case_list {
                            if !matches!(
                                case.status,
                                TestCaseStatus::Failed | TestCaseStatus::Error
                            ) {
                                continue;
                            }
                            let status = if case.status == TestCaseStatus::Error {
                                "ERROR"
                            } else {
                                "FAILED"
                            };
                            println!(
                                "    {:>8}  {}#{}  {}  ({label})",
                                status,
                                case.class_name,
                                case.name,
                                case.message.as_deref().unwrap_or("no message"),
                            );
                        }
                    }
                }
                let summary = format!(
                    "{pass} passed · {fail} failed · {other} other ({})",
                    per_target.len()
                );
                println!("{}", summary);
            }
            Ok(exit_for(
                "test",
                if failed && result.code == 0 {
                    3
                } else {
                    result.code
                },
            ))
        }
        Command::Run { label, passthrough } => {
            let runner = runner_for(cli, false).await?;
            let result = runner
                .exec_run(
                    &load_pv(&runner, None)?,
                    &spec(vec![label.clone()], passthrough.clone()),
                )
                .await?;
            Ok(exit_for("run", result.code))
        }
        Command::Coverage {
            labels,
            passthrough,
        } => {
            let runner = runner_for(cli, false).await?;
            let snapshot = load_snapshot(workspace_of(cli)?)?;
            let labels = default_labels(&snapshot, labels)?;
            let result = runner
                .exec_coverage(
                    &load_pv(&runner, None)?,
                    &spec(labels.clone(), passthrough.clone()),
                )
                .await?;
            present_exec("coverage", &result, true);
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
        Command::Recipes { run, json } => {
            let runner = runner_for(cli, false).await?;
            let pv = load_pv(&runner, cli.project_view.as_ref())?;
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
                    println!(
                        "{}: {} {}  ({} env)",
                        r.name,
                        r.command,
                        r.target,
                        r.env.len()
                    );
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
                    eprintln!(
                        "[baztet] recipe '{}': {} {}",
                        recipe.name, recipe.command, recipe.target
                    );
                    let mut spec = ExecSpec {
                        labels: vec![recipe.target.clone()],
                        env: recipe.env.clone(),
                        ..Default::default()
                    };
                    if recipe.command == "test" {
                        // acceptance/unit recipes run their target; no extra filter
                        let result = runner.exec_test(&pv, &spec).await?;
                        present_exec("test", &result, false);
                        Ok(exit_for("test", result.code))
                    } else if recipe.command == "build" {
                        let result = runner.exec_build(&pv, &spec).await?;
                        present_exec("build", &result, true);
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
                    writeln!(
                        report,
                        "bazel: {} ({})",
                        binary,
                        String::from_utf8_lossy(&out.stdout).trim()
                    )?;
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
            match baztet_core::workspace::find_project_view(&ws) {
                Some(pv_path) => writeln!(report, "project view: {pv_path}")?,
                None => writeln!(
                    report,
                    "project view: not present (tried {})",
                    baztet_core::workspace::PV_CANDIDATES.join(", ")
                )?,
            }
            // snapshot
            match baztet_snapshot::Snapshot::load(&ws) {
                Ok(snapshot) => writeln!(
                    report,
                    "snapshot: {} target(s), {} ide entries{}, last fingerprint {:?}",
                    snapshot.graph.len(),
                    snapshot.ide_infos.len(),
                    if snapshot.has_error {
                        " (has errors)"
                    } else {
                        ""
                    },
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
        Command::Deps {
            label,
            reverse,
            transitive,
        } => {
            if !label.starts_with("//") && !label.starts_with('@') {
                anyhow::bail!("not a bazel label: {label}");
            }
            let snapshot = load_snapshot(workspace_of(cli)?)?;
            let Some(idx) = snapshot.graph.index_of(label) else {
                anyhow::bail!("target {label} not in snapshot; run `baztet sync` first");
            };
            let graph = &snapshot.graph;
            let labels: Vec<String> = if *reverse {
                if !*transitive {
                    let mut out: Vec<String> = graph
                        .reverse_deps_indices(idx)
                        .into_iter()
                        .filter_map(|i| graph.get_by_index(i).map(|t| t.label.clone()))
                        .collect();
                    out.sort();
                    out
                } else {
                    let mut out = vec![];
                    let mut seen = HashSet::new();
                    let mut queue = vec![idx];
                    let mut pos = 0;
                    while pos < queue.len() {
                        let node_idx = queue[pos];
                        pos += 1;
                        for dep_idx in graph.reverse_deps_indices(node_idx) {
                            if seen.insert(dep_idx) {
                                out.push(graph.get_by_index(dep_idx).unwrap().label.clone());
                                queue.push(dep_idx);
                            }
                        }
                    }
                    out.sort();
                    out
                }
            } else {
                let direct = &graph.get_by_index(idx).unwrap().deps;
                let indices = if *transitive {
                    graph.transitive_deps(idx)
                } else {
                    direct.clone()
                };
                indices
                    .into_iter()
                    .skip(if *transitive { 1 } else { 0 })
                    .filter_map(|i| graph.get_by_index(i).map(|t| t.label.clone()))
                    .collect()
            };
            for l in &labels {
                println!("{l}");
            }
            // rule inputs outside the graph: external deps, generated files
            if !*reverse {
                for u in graph.unlinked_deps(idx) {
                    println!("# unlinked: {u}");
                }
            }
            Ok(ExitCode::SUCCESS)
        }
        Command::Bazel(args) => exec_bazel_passthrough(cli, args.clone()).await,
    }
}

fn default_labels(
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

fn load_pv(
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

/// Project view for exec commands; empty when the workspace has none.
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

/// Runner plus tuning flags (`--no-output-check`) and quiet mode.
async fn runner_for(cli: &Cli, refresh: bool) -> anyhow::Result<BazelRunner> {
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
async fn discover(cli: &Cli, refresh: bool) -> anyhow::Result<BazelRunner> {
    let ws = workspace_of(cli)?;
    // --bazel wins; else the project view's `bazel_binary`; else `bazel`
    let binary = match &cli.bazel {
        Some(b) => b.clone(),
        None => baztet_core::workspace::find_project_view(&ws)
            .and_then(|p| parse_pv_soft(&p, &ws).map(|pv| pv.bazel_binary().to_owned()))
            .unwrap_or_else(|| "bazel".to_string()),
    };
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

/// Quiet-UI post-processing of a captured run: warnings surface, failures
/// dump raw bazel output (`--raw`/`run` streamed already). `test` calls
/// with show_status=false — its verdict summary and exit 3 own the report.
fn present_exec(command: &str, result: &baztet_core::exec::ExecResult, show_status: bool) {
    let Some(out) = &result.output else {
        return;
    };
    let warns = baztet_core::exec::warn_lines(out);
    for w in warns.iter().take(30) {
        warn(w);
    }
    if warns.len() > 30 {
        warn(&format!(
            "... {} more warning lines suppressed",
            warns.len() - 30
        ));
    }
    if result.code == 0 && show_status {
        ok(&format!("{command} ok ({:.1}s)", result.secs));
    }
    if result.code != 0 && !show_status {
        // exit 3 (failed verdicts) and its report belong to the caller
        fail(&format!("bazel exited with code {}", result.code));
    }
    if result.code != 0 {
        let raw = if out.stderr.is_empty() {
            &out.stdout
        } else {
            &out.stderr
        };
        let text = String::from_utf8_lossy(raw);
        let lines: Vec<&str> = text.lines().collect();
        let tail = &lines[lines.len().saturating_sub(400)..];
        for line in tail {
            eprintln!("{line}");
        }
    }
}
