//! stderr presentation; --json and machine outputs never touch this module.

use owo_colors::OwoColorize;
use owo_colors::Stream;

use baztet_core::exec::ExecResult;

pub fn ok(msg: &str) {
    eprintln!(
        "{}",
        format!("✓ {msg}").if_supports_color(Stream::Stderr, |t| t.green().to_string())
    );
}

pub fn warn(msg: &str) {
    eprintln!(
        "{}",
        format!("! {msg}").if_supports_color(Stream::Stderr, |t| t.yellow().to_string())
    );
}

pub fn fail(msg: &str) {
    eprintln!(
        "{}",
        format!("✗ {msg}").if_supports_color(Stream::Stderr, |t| t.red().to_string())
    );
}

pub fn phase(n: u8, total: u8, label: &str, secs: f64) {
    eprintln!(
        "{}",
        format!("[{n}/{total}] {label} ({secs:.1}s)")
            .if_supports_color(Stream::Stderr, |t| t.blue().to_string())
    );
}

pub fn test_status_colored(status: &str) -> String {
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
pub fn status_is_failure(status: &str) -> bool {
    status != "PASSED"
}

/// Quiet-UI post-processing of a captured run: warnings surface, failures
/// dump raw bazel output (`--raw`/`run` streamed already). `test` calls
/// with show_status=false — its verdict summary and exit 3 own the report.
pub fn present_exec(command: &str, result: &ExecResult, show_status: bool) {
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

/// The doctor report body plus its ok flag — the only place in the CLI
/// that inspects the environment instead of running a bazel command.
/// Spawns the bazel binary for a version probe.
pub fn doctor_report(
    ws: &camino::Utf8PathBuf,
    cli_bazel: Option<&str>,
) -> anyhow::Result<(String, bool)> {
    use std::fmt::Write as DocWrite;
    let mut report = String::new();
    let mut ok = true;

    writeln!(report, "baztet doctor")?;
    // bazel
    let binary = cli_bazel.unwrap_or("bazel");
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
    writeln!(report, "workspace: {ws}")?;
    // project view
    match baztet_core::workspace::find_project_view(ws) {
        Some(pv_path) => writeln!(report, "project view: {pv_path}")?,
        None => writeln!(
            report,
            "project view: not present (tried {})",
            baztet_core::workspace::PV_CANDIDATES.join(", ")
        )?,
    }
    // snapshot
    match baztet_snapshot::Snapshot::load(ws) {
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
        false => writeln!(
            report,
            "aspect: not deployed yet (first sync) and .bazelbsp (IDE) state unknown; not an error"
        )?,
    }
    Ok((report, ok))
}
