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
        /// fail on sync errors instead of saving a partial snapshot
        #[arg(long)]
        fresh: bool,
        /// stop after the fast query phase (skip aspect analysis)
        #[arg(long)]
        fast: bool,
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
    match &cli.command {
        Command::Sync { project_view, json, fresh, fast } => {
            let runner = discover(cli).await?;
            let pv_path =
                project_view.clone().unwrap_or_else(|| runner.workspace_root.join(".bazelproject"));
            if !pv_path.is_file() {
                anyhow::bail!("project view not found at {pv_path}");
            }
            let pv = baztet_projectview::parse(&pv_path, &runner.workspace_root)?;
            let snapshot = sync_engine::sync(&runner, &pv, !fast).await?;
            let path = snapshot.save()?;
            eprintln!(
                "[baztet] synced {} target(s), {} -> {path}",
                snapshot.graph.len(),
                if snapshot.has_error { "with errors" } else { "ok" },
            );
            if *json {
                println!("{}", snapshot.to_json(true)?);
            }
            if *fresh && snapshot.has_error {
                return Ok(exit_code(1));
            }
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
            let snapshot = load_snapshot(workspace_of(&cli)?)?;
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