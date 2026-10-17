//! End-to-end integration test against the real bazel (9.x) and the
//! `fixtures/simple` workspace. Skipped when bazel is not available.

use std::process::{Command, Stdio};

fn bazet_binary() -> &'static str {
    env!("CARGO_BIN_EXE_baztet")
}

fn run_bazet(args: &[&str]) -> std::process::Output {
    let ws = concat!(env!("CARGO_MANIFEST_DIR"), "/../../fixtures/simple");
    Command::new(bazet_binary())
        .args(["--workspace", ws])
        .args(args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .expect("baztet binary")
}

fn bazel_available() -> bool {
    Command::new("bazel")
        .arg("--version")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

fn fixture_tests() -> bool {
    let ws = concat!(env!("CARGO_MANIFEST_DIR"), "/../../fixtures/simple");
    std::path::Path::new(ws).join("MODULE.bazel").is_file()
}

#[test]
#[ignore = "requires bazel; run with `cargo test -p baztet -- --ignored`"]
fn full_query_sync() {
    if !bazel_available() || !fixture_tests() {
        return;
    }

    // clean previous snapshot
    let ws = concat!(env!("CARGO_MANIFEST_DIR"), "/../../fixtures/simple");
    let _ = std::fs::remove_file(format!("{ws}/.baztet/snapshot.bin"));

    let out = run_bazet(&["sync"]);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "sync failed:\n{stderr}");
    assert!(stderr.contains("3 target(s)"), "unexpected output:\n{stderr}");

    let out = run_bazet(&["targets"]);
    let stdout = String::from_utf8_lossy(&out.stdout);
    for expected in ["//:bin_b [genrule]", "//:hello [genrule]", "//:lib_a [genrule]"] {
        assert!(stdout.contains(expected), "missing {expected} in:\n{stdout}");
    }

    let out = run_bazet(&["deps", "--transitive", "//:bin_b"]);
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert_eq!(stdout.trim(), "//:lib_a");

    let out = run_bazet(&["deps", "--reverse", "//:lib_a"]);
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert_eq!(stdout.trim(), "//:bin_b");
}