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

fn fixture_exists() -> bool {
    let ws = concat!(env!("CARGO_MANIFEST_DIR"), "/../../fixtures/simple");
    std::path::Path::new(ws).join("MODULE.bazel").is_file()
}

/// Restores BAZTET_CACHE_DIR and removes the throwaway cache dir on drop:
/// process-global env must not leak into later tests.
struct CacheEnv {
    _prev: Option<std::ffi::OsString>,
    dir: std::path::PathBuf,
}

impl Drop for CacheEnv {
    fn drop(&mut self) {
        match &self._prev {
            Some(v) => std::env::set_var("BAZTET_CACHE_DIR", v),
            None => std::env::remove_var("BAZTET_CACHE_DIR"),
        }
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

#[test]
#[ignore = "requires bazel; run with `cargo test -p baztet -- --ignored`"]
fn full_sync_and_execution() {
    if !bazel_available() || !fixture_exists() {
        return;
    }

    // cold sync: point the snapshot cache at a throwaway dir (snapshots
    // live OUTSIDE the workspace since 0.3.0)
    let cache = std::env::temp_dir().join(format!("baztet-test-cache-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&cache);
    let _env = CacheEnv {
        _prev: std::env::var("BAZTET_CACHE_DIR")
            .ok()
            .map(std::ffi::OsString::from),
        dir: cache.clone(),
    };
    std::env::set_var("BAZTET_CACHE_DIR", &cache);

    // full sync (query + aspect)
    let out = run_bazet(&["sync"]);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "sync failed:\n{stderr}");
    assert!(
        stderr.contains("3 target(s)"),
        "unexpected output:\n{stderr}"
    );

    let out = run_bazet(&["targets"]);
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("//:hello [sh_binary]"),
        "missing hello in:\n{stdout}"
    );
    assert!(
        stdout.contains("//:unit_test [sh_test]"),
        "missing unit_test in:\n{stdout}"
    );
    assert!(
        stdout.contains("//:failing_test [sh_test]"),
        "missing failing_test in:\n{stdout}"
    );

    // build everything
    let out = run_bazet(&["build"]);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "build failed:\n{stderr}");

    // passing test via recipe (import_run_configurations from the project view)
    let out = run_bazet(&["recipes"]);
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("simple unit test: test //:unit_test"),
        "recipes:\n{stdout}"
    );

    let out = run_bazet(&["recipes", "--run", "simple unit test"]);
    assert!(out.status.success());

    let out = run_bazet(&["test", "--pattern", "unit"]);
    assert!(out.status.success(), "passing test failed");
    let stdout = String::from_utf8_lossy(&out.stdout);
    // everything green: totals only, no per-target detail
    assert!(stdout.contains("1 of 1 target(s) passed"), "quiet-ok line missing:\n{stdout}");
    assert!(!stdout.contains("PASSED   "), "per-target detail leaked on a green run:\n{stdout}");

    // failing test: exit code 3 (tests failed), FAILED status printed
    let out = run_bazet(&["test", "--pattern", "failing"]);
    assert!(!out.status.success(), "failing test must fail");
    assert_eq!(out.status.code(), Some(3), "failing test exit code");
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("FAILED"), "missing FAILED in:\n{stdout}");

    // json report
    let out = run_bazet(&["test", "--pattern", "unit", "--json"]);
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("\"PASSED\""), "json report:\n{stdout}");

    // run streams the binary output
    let out = run_bazet(&["run", "//:hello"]);
    assert!(out.status.success(), "run failed");
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("hello"), "run output:\n{stdout}");

    // incremental: touch a fixture BUILD, sync again — the delta aspect
    // path must re-sync the same targets without error
    let build = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../fixtures/simple/BUILD.bazel"
    );
    std::fs::File::options()
        .append(true)
        .open(build)
        .unwrap()
        .set_modified(std::time::SystemTime::now())
        .unwrap();
    let out = run_bazet(&["sync"]);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "second sync failed:\n{stderr}");
    assert!(
        stderr.contains("3 target(s)"),
        "unexpected output:\n{stderr}"
    );
}
