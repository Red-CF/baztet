//! One function per CLI subcommand; `main.rs` only dispatches.

use std::io::IsTerminal;
use std::process::ExitCode;

use crate::ctx::{
    default_labels, exit_code, exit_for, load_pv, load_snapshot, runner_for, spec, test_labels,
    workspace_of, Ctx,
};
use crate::tui;
use crate::ui;
use crate::Cli;
use baztet_core::sync as sync_engine;

pub async fn sync_cmd(cli: &Cli, json: bool, fresh: bool, fast: bool) -> anyhow::Result<ExitCode> {
    let runner = runner_for(cli, true).await?;
    let pv_path = match &cli.project_view {
        Some(p) => {
            if !p.is_file() {
                anyhow::bail!("project view not found at {p}");
            }
            p.clone()
        }
        None => {
            baztet_core::workspace::find_project_view(&runner.workspace_root).ok_or_else(|| {
                anyhow::anyhow!(
                    "no project view; tried {}. create one, or pass --project-view",
                    baztet_core::workspace::PV_CANDIDATES.join(", ")
                )
            })?
        }
    };
    let pv = baztet_projectview::parse(&pv_path, &runner.workspace_root)?;
    let report = sync_engine::sync(&runner, &pv, !fast, fresh).await?;
    let snapshot = &report.snapshot;
    let path = snapshot.save()?;

    match report.reuse {
        sync_engine::Reuse::Everything => ui::warn(&format!(
            "nothing changed: snapshot reused ({} target(s), {} ide entries)",
            snapshot.graph.len(),
            snapshot.ide_infos.len()
        )),
        sync_engine::Reuse::Graph => {
            ui::phase(
                1,
                2,
                "query graph reused",
                report.query_secs.unwrap_or_default(),
            );
            ui::phase(
                2,
                2,
                "aspect outputs",
                report.aspect_secs.unwrap_or_default(),
            );
        }
        sync_engine::Reuse::FullQuery => {
            ui::phase(1, 2, "query", report.query_secs.unwrap_or_default());
            // the aspect phase prints only when it ran (--fast skips it)
            if let Some(secs) = report.aspect_secs {
                ui::phase(2, 2, "aspect build + BEP", secs);
            }
        }
    }

    let line = format!(
        "synced {} target(s) · {} ide entries · snapshot {path}",
        snapshot.graph.len(),
        snapshot.ide_infos.len()
    );
    if snapshot.has_error {
        ui::fail(&format!("{line} (some outputs failed)"));
    } else {
        ui::ok(&line);
    }
    if json {
        println!("{}", snapshot.to_json(true)?);
    }
    Ok(ExitCode::SUCCESS)
}

pub async fn build_cmd(
    cli: &Cli,
    labels: &[String],
    passthrough: Vec<String>,
) -> anyhow::Result<ExitCode> {
    let ctx = Ctx::exec(cli, false, true).await?;
    let labels = default_labels(ctx.snapshot()?, labels)?;
    let build_spec = spec(labels, passthrough);
    let result = if dashboard(cli) {
        // the dashboard is how a build runs on a terminal
        let child = ctx
            .runner
            .exec_build_fork(ctx.pv(), &build_spec, true)
            .await?;
        tui::run(child, "build").await?
    } else {
        ctx.runner.exec_build(ctx.pv(), &build_spec).await?
    };
    ui::present_exec("build", &result, true);
    Ok(exit_for("build", result.code))
}

// the mirror of clap's Test flags; grouping them would just restate them
#[allow(clippy::too_many_arguments)]
pub async fn test_cmd(
    cli: &Cli,
    labels: &[String],
    filter: &Option<String>,
    pattern: &Option<String>,
    cases: bool,
    json: bool,
    passthrough: Vec<String>,
) -> anyhow::Result<ExitCode> {
    let ctx = Ctx::exec(cli, false, true).await?;
    let labels = test_labels(ctx.snapshot()?, labels, pattern)?;
    let mut test_spec = spec(labels.clone(), passthrough);
    test_spec.test_filter = filter.clone();
    // the live dashboard is the only way `test` runs on a terminal; --raw
    // and non-tty callers (pipes, CI) get the quiet/raw capture instead
    let result = if dashboard(cli) {
        let child = ctx
            .runner
            .exec_test_fork(ctx.pv(), &test_spec, true)
            .await?;
        tui::run(child, "test").await?
    } else {
        ctx.runner.exec_test(ctx.pv(), &test_spec).await?
    };
    ui::present_exec("test", &result, false);

    let bep = result
        .bep
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("no BEP output"))?;
    let per_target: Vec<(&String, &String)> = bep
        .test_summaries
        .iter()
        .map(|s| (&s.label, &s.status))
        .collect();
    let parsed = baztet_core::exec::test_cases_from_bep(&ctx.runner, bep);
    let failed = result.code != 0
        || bep
            .test_summaries
            .iter()
            .any(|s| ui::status_is_failure(&s.status));

    if json {
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
        let (mut pass, mut failed_cnt, mut other) = (0usize, 0usize, 0usize);
        for (label, status) in &per_target {
            let _ = label;
            match status.as_str() {
                "PASSED" => pass += 1,
                // test verdicts that count against the exit code;
                // anything else (CANCELLED, NO_STATUS, ...) is
                // neither passed nor failed by a verdict
                "FAILED" | "TIMEOUT" | "FLAKY" => failed_cnt += 1,
                _ => other += 1,
            }
        }
        // everything green: just the totals; detail belongs to the run
        // that failed (per-target rows + case reports below)
        if !failed {
            println!(
                "test ok: {pass} of {} target(s) passed, 0 failed",
                per_target.len()
            );
        }
        if failed {
            // per-target rows only for the verdicts that explain the exit:
            // the passing N targets live in the totals line
            for (label, status) in &per_target {
                if status.as_str() != "PASSED" {
                    println!("{}  {label}", ui::test_status_colored(status));
                }
            }
        }
        if failed && cases {
            use baztet_core::exec::TestCaseStatus;
            for (label, case_list) in &parsed {
                for case in case_list {
                    if !matches!(case.status, TestCaseStatus::Failed | TestCaseStatus::Error) {
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
        if failed {
            println!(
                "{pass} passed · {failed_cnt} failed · {other} other ({})",
                per_target.len()
            );
        }
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

pub async fn run_cmd(
    cli: &Cli,
    label: String,
    passthrough: Vec<String>,
) -> anyhow::Result<ExitCode> {
    let ctx = Ctx::exec(cli, false, false).await?;
    let result = ctx
        .runner
        .exec_run(ctx.pv(), &spec(vec![label], passthrough))
        .await?;
    Ok(exit_for("run", result.code))
}

pub async fn coverage_cmd(
    cli: &Cli,
    labels: &[String],
    passthrough: Vec<String>,
) -> anyhow::Result<ExitCode> {
    let ctx = Ctx::exec(cli, false, true).await?;
    let labels = default_labels(ctx.snapshot()?, labels)?;
    let cov_spec = spec(labels, passthrough);
    let result = if dashboard(cli) {
        let child = ctx
            .runner
            .exec_coverage_fork(ctx.pv(), &cov_spec, true)
            .await?;
        tui::run(child, "build").await?
    } else {
        ctx.runner.exec_coverage(ctx.pv(), &cov_spec).await?
    };
    ui::present_exec("coverage", &result, true);
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

pub async fn recipes_cmd(cli: &Cli, run: Option<String>, json: bool) -> anyhow::Result<ExitCode> {
    use baztet_core::exec::Recipe;

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
    if json {
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
            let mut exec_spec = baztet_core::exec::ExecSpec {
                labels: vec![recipe.target.clone()],
                env: recipe.env.clone(),
                ..Default::default()
            };
            if recipe.command == "test" {
                // acceptance/unit recipes run their target; no extra filter
                let result = if dashboard(cli) {
                    let child = runner.exec_test_fork(&pv, &exec_spec, true).await?;
                    tui::run(child, "test").await?
                } else {
                    runner.exec_test(&pv, &exec_spec).await?
                };
                ui::present_exec("test", &result, false);
                Ok(exit_for("test", result.code))
            } else if recipe.command == "build" {
                let result = if dashboard(cli) {
                    let child = runner.exec_build_fork(&pv, &exec_spec, true).await?;
                    tui::run(child, "build").await?
                } else {
                    runner.exec_build(&pv, &exec_spec).await?
                };
                ui::present_exec("build", &result, true);
                Ok(exit_for("build", result.code))
            } else if recipe.command == "run" {
                exec_spec
                    .extra_args
                    .push("--single_top_level_target".into());
                let result = runner.exec_run(&pv, &exec_spec).await?;
                Ok(exit_for("run", result.code))
            } else {
                anyhow::bail!("unsupported recipe command: {}", recipe.command);
            }
        }
        None => Ok(ExitCode::SUCCESS),
    }
}

pub fn doctor_cmd(cli: &Cli) -> anyhow::Result<ExitCode> {
    let ws = workspace_of(cli)?;
    let (report, ok) = ui::doctor_report(&ws, cli.bazel.as_deref())?;
    print!("{report}");
    Ok(if ok { ExitCode::SUCCESS } else { exit_code(1) })
}

pub async fn bench_cmd(targets: u64) -> anyhow::Result<ExitCode> {
    let result = baztet_core::bench::run(targets).await?;
    eprintln!(
        "[baztet] decoded {} targets in {:?}; 10k-target snapshot roundtrip {:?}",
        result.decoded, result.decode_duration, result.snapshot_roundtrip
    );
    Ok(ExitCode::SUCCESS)
}

pub fn targets_cmd(cli: &Cli, filter: &Option<String>, json: bool) -> anyhow::Result<ExitCode> {
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
    if json {
        println!("{}", serde_json::to_string_pretty(&targets)?);
    } else {
        for t in targets {
            println!("{} [{}]", t.label, t.kind);
        }
    }
    Ok(ExitCode::SUCCESS)
}

pub fn deps_cmd(
    cli: &Cli,
    label: String,
    reverse: bool,
    transitive: bool,
) -> anyhow::Result<ExitCode> {
    use std::collections::HashSet;

    if !label.starts_with("//") && !label.starts_with('@') {
        anyhow::bail!("not a bazel label: {label}");
    }
    let snapshot = load_snapshot(workspace_of(cli)?)?;
    let Some(idx) = snapshot.graph.index_of(&label) else {
        anyhow::bail!("target {label} not in snapshot; run `baztet sync` first");
    };
    let graph = &snapshot.graph;
    let labels: Vec<String> = if reverse {
        if !transitive {
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
        let indices = if transitive {
            graph.transitive_deps(idx)
        } else {
            direct.clone()
        };
        indices
            .into_iter()
            .skip(if transitive { 1 } else { 0 })
            .filter_map(|i| graph.get_by_index(i).map(|t| t.label.clone()))
            .collect()
    };
    for l in &labels {
        println!("{l}");
    }
    // rule inputs outside the graph: external deps, generated files
    if !reverse {
        for u in graph.unlinked_deps(idx) {
            println!("# unlinked: {u}");
        }
    }
    Ok(ExitCode::SUCCESS)
}

/// passthrough: `baztet <anything-not-a-baztet-command>` = bazel with the
/// same args, in the discovered workspace, streamed. --raw-ish by design.
pub async fn passthrough_cmd(cli: &Cli, args: Vec<String>) -> anyhow::Result<ExitCode> {
    let ws = workspace_of(cli)?;
    let binary = crate::ctx::resolve_binary(cli, &ws);
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

/// The dashboard decides itself: interactive stdout AND stdin (crossterm's
/// event source), no --raw. Pipes/CI and wrappers with closed stdin get
/// quiet capture; a piped stdin must never kill a build via its bytes.
fn dashboard(cli: &Cli) -> bool {
    !cli.raw && std::io::stdout().is_terminal() && std::io::stdin().is_terminal()
}

#[cfg(test)]
mod tests {
    use super::*;
    use baztet_snapshot::Snapshot;
    use camino::Utf8PathBuf;
    use clap::Parser;

    /// env writes are process-global: serialize the two tests that touch it
    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn fixture_workspace(tag: &str) -> (Utf8PathBuf, Utf8PathBuf) {
        let dir = std::env::temp_dir().join(format!("baztet-cmds-{tag}-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("MODULE.bazel"), "").unwrap();
        (
            Utf8PathBuf::from_path_buf(dir.clone()).unwrap(),
            Utf8PathBuf::from_path_buf(dir).unwrap(),
        )
    }

    #[test]
    fn targets_lists_and_filters() {
        let _guard = ENV_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let (ws, dir) = fixture_workspace("targets");
        std::env::set_var("BAZTET_CACHE_DIR", &dir);
        let mut snap = Snapshot::new(ws.clone());
        snap.graph
            .get_or_insert("//a:lib", "sh_library", None, vec![]);
        snap.graph
            .get_or_insert("//b:test", "sh_test", None, vec![]);
        snap.save().unwrap();
        let cli = Cli::parse_from(["baztet", "--workspace", ws.as_str(), "targets"]);
        let out = targets_cmd(&cli, &None, false).unwrap();
        assert_eq!(out, ExitCode::SUCCESS);
        // json path too
        targets_cmd(&cli, &Some("=sh_test".into()), true).unwrap();
        std::env::remove_var("BAZTET_CACHE_DIR");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn deps_guards_and_lists() {
        let _guard = ENV_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let (ws, dir) = fixture_workspace("deps");
        std::env::set_var("BAZTET_CACHE_DIR", &dir);
        let cli = Cli::parse_from(["baztet", "--workspace", ws.as_str(), "deps", "//a:lib"]);
        // not a label -> error
        assert!(deps_cmd(&cli, "notalabel".into(), false, false).is_err());
        // not in snapshot -> error
        let mut snap = Snapshot::new(ws.clone());
        snap.graph
            .get_or_insert("//a:lib", "sh_library", None, vec![]);
        let b = snap
            .graph
            .get_or_insert("//a:dep", "sh_library", None, vec![]);
        let a = snap
            .graph
            .get_or_insert("//a:lib2", "sh_library", None, vec![]);
        snap.graph.add_dep(a, b);
        snap.save().unwrap();
        // not in the graph -> error
        assert!(deps_cmd(&cli, "//a:bogus".into(), false, false).is_err());
        assert_eq!(
            deps_cmd(&cli, "//a:lib2".into(), false, false).unwrap(),
            ExitCode::SUCCESS
        );
        assert_eq!(
            deps_cmd(&cli, "//a:lib2".into(), true, true).unwrap(),
            ExitCode::SUCCESS
        );
        std::env::remove_var("BAZTET_CACHE_DIR");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
