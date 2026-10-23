//! Opt-in integration test against a REAL Bazel workspace (the daily-driver
//! validation path). Never runs in CI; skips unless:
//!
//!   BAZTET_REAL_WS=/path/to/workspace   (workspace root, required)
//!   BAZTET_REAL_PV=/path/to/.bazelproject (optional custom project view)
//!
//! Asserts only shape, not repo content: sync succeeds, targets non-empty,
//! deps of a target work. Nothing about the real repo is committed here.
//!
//! Run: `BAZTET_REAL_WS=... cargo test -p baztet --real -- --ignored`

use std::process::{Command, Stdio};

fn env_or_none(k: &str) -> Option<String> {
    std::env::var(k).ok().filter(|v| !v.is_empty())
}

fn runner(args: &[String]) -> Command {
    let ws = env_or_none("BAZTET_REAL_WS").unwrap_or_default();
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_baztet"));
    cmd.args(["--workspace", &ws]);
    if let Some(pv) = env_or_none("BAZTET_REAL_PV") {
        cmd.args(["--project-view", &pv]);
    }
    cmd.args(args).stdout(Stdio::piped()).stderr(Stdio::piped());
    cmd
}

#[test]
#[ignore = "opt-in: only runs when BAZTET_REAL_WS is set"]
fn real_workspace_sync_and_query() {
    let Some(_) = env_or_none("BAZTET_REAL_WS") else {
        return;
    };

    // full sync (query + aspect); heavy but this is the daily path
    let out = runner(&["sync".into(), "--fresh".into()]).output().unwrap();
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "fresh sync failed:\n{stderr}");

    // at least one target synced
    let out = runner(&["targets".into()]).output().unwrap();
    assert!(out.status.success(), "targets failed");
    let stdout = String::from_utf8_lossy(&out.stdout);
    let first = stdout
        .lines()
        .next()
        .expect("no targets in snapshot")
        .trim()
        .to_string();
    assert!(first.contains("//"), "unexpected target line: {first}");

    // deps of the smallest target resolve without error (may be empty)
    let label = stdout
        .lines()
        .next()
        .unwrap()
        .split(" [")
        .next()
        .unwrap()
        .trim()
        .to_string();
    let out = runner(&["deps".into(), label.clone()]).output().unwrap();
    assert!(out.status.success(), "deps {label} failed");

    // fast sync reuses the graph
    let out = runner(&["sync".into(), "--fast".into()]).output().unwrap();
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("target(s)") || out.status.success(),
        "fast sync failed:\n{stderr}"
    );
}
