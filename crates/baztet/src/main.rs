use std::process::ExitCode;

#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

use baztet_core::BazelError;
use camino::Utf8PathBuf;
use clap::{Parser, Subcommand};

mod commands;
mod ctx;
mod tui;
mod ui;

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
    match dispatch(cli).await {
        Ok(code) => code,
        Err(e) => {
            ui::fail(&format!("{e}"));
            for cause in e.chain().skip(1) {
                eprintln!("  ↳ {cause}");
            }
            for cause in e.chain() {
                if let Some(BazelError::Failure(_, code, _)) = cause.downcast_ref() {
                    return ctx::exit_code(*code);
                }
            }
            ExitCode::FAILURE
        }
    }
}

async fn dispatch(cli: &Cli) -> anyhow::Result<ExitCode> {
    match &cli.command {
        Command::Sync { json, fresh, fast } => commands::sync_cmd(cli, *json, *fresh, *fast).await,
        Command::Build {
            labels,
            passthrough,
        } => commands::build_cmd(cli, labels, passthrough.clone()).await,
        Command::Test {
            labels,
            filter,
            pattern,
            cases,
            json,
            tui,
            passthrough,
        } => {
            commands::test_cmd(
                cli,
                labels,
                filter,
                pattern,
                *cases,
                *json,
                *tui,
                passthrough.clone(),
            )
            .await
        }
        Command::Run { label, passthrough } => {
            commands::run_cmd(cli, label.clone(), passthrough.clone()).await
        }
        Command::Coverage {
            labels,
            passthrough,
        } => commands::coverage_cmd(cli, labels, passthrough.clone()).await,
        Command::Recipes { run, json } => commands::recipes_cmd(cli, run.clone(), *json).await,
        Command::Doctor => commands::doctor_cmd(cli),
        Command::Bench { targets } => commands::bench_cmd(*targets).await,
        Command::Targets { filter, json } => commands::targets_cmd(cli, filter, *json),
        Command::Deps {
            label,
            reverse,
            transitive,
        } => commands::deps_cmd(cli, label.clone(), *reverse, *transitive),
        Command::Bazel(args) => commands::passthrough_cmd(cli, args.clone()).await,
    }
}
