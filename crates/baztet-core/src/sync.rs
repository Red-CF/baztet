//! Sync pipeline: query phase (see [`crate::query`]) + aspect/BEP phase, the
//! incremental reuse ladder (see [`crate::reuse`]), and snapshot merging.

use std::collections::{BTreeMap, BTreeSet};

use baztet_bep::BepOutput;
use baztet_projectview::ProjectView;
use baztet_snapshot::Snapshot;

use camino::Utf8PathBuf;

use crate::aspect::{self, AspectConfig, Rules};
use crate::bazel::{BazelError, BazelRunner};
use crate::expand::expand_directory_targets;
pub use crate::query::{batch_specs, target_specs, DEFAULT_SHARD_SIZE};
pub use crate::reuse::{decide_reuse, sync_fingerprint, Reuse};

/// Full sync; `run_aspect_phase=false` stops after the query phase, `force_full` skips the reuse ladder.
pub async fn sync(
    runner: &BazelRunner,
    pv: &ProjectView,
    run_aspect_phase: bool,
    force_full: bool,
) -> Result<crate::reuse::SyncReport, BazelError> {
    let t0 = std::time::Instant::now();

    let existing = if force_full {
        None
    } else {
        // a missing or corrupt snapshot just falls through to a full query
        Snapshot::load(&runner.workspace_root).ok()
    };

    if let Some(existing) = existing {
        let specs = target_specs(pv, runner.workspace_root.as_str());
        let current_fp = sync_fingerprint(&specs, pv, runner);
        let recorded_fp = existing.sync_fingerprint;
        let decision = decide_reuse(
            recorded_fp == Some(current_fp) && !existing.graph.is_empty(),
            existing.build_files_mtime,
            max_build_mtime(pv, &runner.workspace_root),
            !pv.directories.entries.is_empty(),
            run_aspect_phase,
            !existing.ide_infos.is_empty(),
        );
        if decision != Reuse::FullQuery {
            let mut snapshot = existing;
            if decision == Reuse::Graph {
                eprintln!("[baztet] project view unchanged; refreshing aspect outputs");
                let aspect_secs = run_aspect_phase_once(runner, pv, &mut snapshot).await;
                let parsed = parse_count();
                snapshot.build_files_mtime = max_build_mtime(pv, &runner.workspace_root);
                return Ok(crate::reuse::SyncReport {
                    snapshot,
                    reuse: Reuse::Graph,
                    // no query ran; report 0 elapsed for phase 1, and the
                    // measured aspect window for phase 2
                    query_secs: Some(0.0),
                    aspect_secs: Some(aspect_secs),
                    parsed,
                });
            }
            return Ok(crate::reuse::SyncReport {
                snapshot,
                reuse: Reuse::Everything,
                query_secs: Some(t0.elapsed().as_secs_f64()),
                aspect_secs: None,
                parsed: 0,
            });
        }
    }

    // full path: query, then optional aspect phase
    let (graph, query_failed) = crate::query::query_graph(runner, pv).await?;
    let query_secs = t0.elapsed().as_secs_f64();
    let mut snapshot = Snapshot::new(runner.workspace_root.clone());
    snapshot.specified_targets = target_specs(pv, runner.workspace_root.as_str());
    snapshot.graph = graph;
    snapshot.has_error = query_failed;
    snapshot.sync_fingerprint = Some(sync_fingerprint(&snapshot.specified_targets, pv, runner));
    snapshot.bazel_version = runner.info.bazel_version().map(str::to_owned);

    let mut aspect_secs = None;
    let mut parsed = 0usize;
    if run_aspect_phase {
        aspect_secs = Some(run_aspect_phase_once(runner, pv, &mut snapshot).await);
        parsed = parse_count();
    }
    snapshot.build_files_mtime = max_build_mtime(pv, &runner.workspace_root);
    Ok(crate::reuse::SyncReport {
        reuse: Reuse::FullQuery,
        snapshot,
        query_secs: Some(query_secs),
        aspect_secs,
        parsed,
    })
}

/// Run the aspect phase and persist its error into the snapshot; returns seconds.
async fn run_aspect_phase_once(
    runner: &BazelRunner,
    pv: &ProjectView,
    snapshot: &mut Snapshot,
) -> f64 {
    let t = std::time::Instant::now();
    match aspect_sync(runner, pv, snapshot).await {
        Ok(()) => {}
        Err(e) => {
            eprintln!("[baztet] aspect sync failed: {e}");
            snapshot.has_error = true;
        }
    }
    t.elapsed().as_secs_f64()
}

/// Max BUILD mtime from the package walk (None when derive is off).
fn max_build_mtime(pv: &ProjectView, workspace_root: &Utf8PathBuf) -> Option<u64> {
    expand_directory_targets(pv, workspace_root.as_str()).1
}

/// Where the aspect gets deployed (relative to the workspace root).
pub const ASPECT_PREFIX: &str = ".baztet/aspect";

/// Rule kinds the aspect modules care about, by language.
pub fn detect_languages(graph: &baztet_snapshot::TargetGraph) -> BTreeSet<Rules> {
    let mut langs: BTreeSet<Rules> = BTreeSet::new();
    for node in graph.iter() {
        let kind = node.kind.as_str();
        if kind.contains("java") {
            langs.insert(Rules::Java);
        }
        if kind.contains("kt") || kind.contains("kotlin") {
            langs.insert(Rules::Kotlin);
        }
        if kind.contains("scala") {
            langs.insert(Rules::Scala);
        }
        if kind.contains("py") || kind.contains("python") {
            langs.insert(Rules::Python);
        }
        if kind.contains("cc") || kind.contains("cpp") {
            langs.insert(Rules::Cc);
        }
        if kind.contains("go") {
            langs.insert(Rules::Go);
        }
        if kind.contains("proto") {
            langs.insert(Rules::Proto);
        }
    }
    langs
}

/// Canonical repo names via `bazel mod dump_repo_mapping`.
fn bazel_repo_mapping(runner: &BazelRunner) -> BTreeMap<String, String> {
    let (code, stdout, stderr) =
        match runner.run_simple_blocking("mod", &["dump_repo_mapping".into(), "".into()]) {
            Ok(v) => v,
            Err(_) => return Default::default(),
        };
    if code != 0 {
        eprintln!("[baztet] dump_repo_mapping failed ({code}):\n{stderr}");
        return Default::default();
    }
    // output is one JSON object (possibly preceded by log lines)
    let json_start = stdout.find('{').unwrap_or(0);
    serde_json::from_str(&stdout[json_start..]).unwrap_or_default()
}

/// Phase 2: aspect sync → TargetIdeInfo summaries.
pub async fn aspect_sync(
    runner: &BazelRunner,
    pv: &ProjectView,
    snapshot: &mut Snapshot,
) -> Result<(), BazelError> {
    let languages = detect_languages(&snapshot.graph);
    let os_name = std::env::consts::OS.to_string();

    // use builtin for the languages bazel ships builtin rules for; remap the
    // rest to their canonical repos
    let mut use_builtin: BTreeSet<Rules> = BTreeSet::new();
    if languages.contains(&Rules::Java) {
        use_builtin.insert(Rules::Java);
    }
    if languages.contains(&Rules::Cc) {
        use_builtin.insert(Rules::Cc);
    }
    if languages.contains(&Rules::Python) {
        use_builtin.insert(Rules::Python);
    }
    let mapping = tokio::task::block_in_place(|| bazel_repo_mapping(runner));
    let mut repo_mapping = std::collections::BTreeMap::new();
    for rule in [
        Rules::Kotlin,
        Rules::Scala,
        Rules::Go,
        Rules::Python,
        Rules::Cc,
    ] {
        if !languages.contains(&rule) {
            continue;
        }
        if use_builtin.contains(&rule) {
            continue;
        }
        let default_name = rule.ruleset_name().trim_start_matches('@');
        if let Some(canonical) = mapping.get(default_name) {
            repo_mapping.insert(rule, canonical.clone());
        }
    }

    let config = AspectConfig {
        bazel_version: runner.info.bazel_version().unwrap_or("").to_string(),
        os_name,
        repo_mapping,
        use_builtin,
    };

    aspect::deploy(&runner.workspace_root, ASPECT_PREFIX, &config)
        .map_err(|e| BazelError::Other(format!("aspect deploy failed: {e}")))?;

    // run the aspect build with a BEP sidecar
    let bep_file = std::env::temp_dir().join(format!("baztet-bep-{}.bin", std::process::id()));
    let mut argv = vec![
        format!("--aspects={}", aspect::aspect_name(ASPECT_PREFIX)),
        "--output_groups=intellij-info".to_string(),
        "--keep_going".to_string(),
        "--norun_validations".to_string(),
        "--build_tag_filters=-no-ide".to_string(),
        format!("--build_event_binary_file={}", bep_file.display()),
        "--build_event_binary_file_upload_mode=wait_for_upload_complete".to_string(),
        "--bes_outerr_buffer_size=10".to_string(),
    ];
    // target patterns for `bazel build` are plain labels, one per argument
    argv.extend(snapshot.specified_targets.iter().cloned());
    argv.extend(pv.sync_flags.iter().cloned());

    let (code, _stdout, stderr) = runner.run_simple("build", &argv).await?;
    let bytes = std::fs::read(&bep_file)
        .map_err(|e| BazelError::Other(format!("failed to read BEP file {bep_file:?}: {e}")))?;
    let _ = std::fs::remove_file(&bep_file);

    let output = baztet_bep::parse(&bytes)
        .map_err(|e| BazelError::Other(format!("failed to parse BEP: {e}")))?;

    // prefer `bazel config --dump_all` (bzlmod config ids); BEP events as fallback
    snapshot.configurations = fetch_configurations(runner, &output).await;

    merge_ide_infos(runner, &output, snapshot);

    if code != 0 {
        eprintln!("[baztet] aspect build exited with code {code}:\n{stderr}");
        snapshot.has_error = true;
    }
    Ok(())
}

/// Configurations from BEP `BuildConfiguration` events (the
/// `bazel config --dump_all` dump is stored upstream-of-need; wire it in
/// when a CLI flag consumes configurations).
///
/// `bazel config` fallback kept behind a function for M5+.
async fn fetch_configurations(
    _runner: &BazelRunner,
    output: &BepOutput,
) -> std::collections::BTreeMap<String, baztet_snapshot::ConfigurationLite> {
    crate::ideinfo::configurations_from_bep(&output.configurations)
}

/// Parse count set by the rayon workers, read by `sync` for SyncReport.parsed.
static PARSE_COUNT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

fn merge_ide_infos(runner: &BazelRunner, output: &BepOutput, snapshot: &mut Snapshot) {
    use rayon::prelude::*;

    PARSE_COUNT.store(0, std::sync::atomic::Ordering::Relaxed);
    let execution_root = runner.info.execution_root.clone();

    // 1) collect the work: (ide entry key, BEP key, file path, mtime)
    let mut jobs: Vec<(String, camino::Utf8PathBuf, Option<std::time::Duration>)> = Vec::new();
    for key in output.target_outputs.keys() {
        let files = output.files_for_target_group(key, baztet_bep::OUTPUT_GROUP_INFO);
        for file in files {
            let relative = file.relative_path();
            let path = execution_root.join(&relative);
            let mtime = aspect_output_mtime(&path);
            let bep_key = format!("{}!{}", key.label, key.configuration);
            // reuse works at BEP-key level: previous sync mapped the same
            // BEP key to the same aspect file
            if file_is_newer(mtime, &relative, snapshot.ide_sources_meta.get(&bep_key)) {
                jobs.push((bep_key, path, mtime));
            }
            // unchanged outputs keep their existing entry as-is
        }
    }
    let bar = indicatif::ProgressBar::new(jobs.len() as u64).with_style(
        indicatif::ProgressStyle::with_template(
            "[baztet] parsing {pos}/{len} aspect outputs ({elapsed})",
        )
        .unwrap(),
    );

    // 2) parse in parallel (indicatif progress is Send, rayon drives the loop);
    //    rayon blocks inside the async runtime worker, so run under
    //    block_in_place to keep the multi-thread executor responsive
    //    (single-thread runtimes would panic here — CLI always uses
    //    multi_thread, and the merge path never runs under one)
    tokio::task::block_in_place(|| {
        type ParseJob = Option<(
            String,
            camino::Utf8PathBuf,
            Option<std::time::Duration>,
            prost_reflect::DynamicMessage,
        )>;
        let parsed: Vec<ParseJob> = jobs
            .into_par_iter()
            .map(|(bep_key, path, mtime)| {
                bar.inc(1);
                let text = match std::fs::read_to_string(&path) {
                    Ok(t) => t,
                    Err(_) => return None,
                };
                let msg = match crate::ideinfo::parse(&text) {
                    Ok(m) => m,
                    Err(e) => {
                        eprintln!("[baztet] skipping unparsable aspect output ({path}): {e}");
                        return None;
                    }
                };
                PARSE_COUNT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                Some((bep_key, path, mtime, msg))
            })
            .collect();

        // 3) merge sequentially (needs &mut snapshot); identity without key: skip
        let mut skipped = 0usize;
        for item in parsed {
            let Some((bep_key, path, mtime, msg)) = item else {
                continue;
            };
            let (identity, mut trimmed) = crate::ideinfo::summarize(&msg);
            let Some((ilabel, iconfig)) = identity else {
                skipped += 1;
                continue;
            };
            let key_label = if ilabel.is_empty() {
                bep_key.split('!').next().unwrap_or("").to_string()
            } else {
                ilabel
            };
            let key_config = if iconfig.is_empty() {
                bep_key.split('!').nth(1).unwrap_or("").to_string()
            } else {
                iconfig
            };
            if let Some(node) = snapshot.graph.get_by_label(&key_label) {
                trimmed.kind = node.kind.clone();
            }
            snapshot
                .ide_infos
                .insert(format!("{key_label}!{key_config}"), trimmed);
            if let Some(mtime) = mtime {
                snapshot.ide_sources_meta.insert(
                    bep_key,
                    format!(
                        "{}@{}",
                        path.strip_prefix(execution_root.as_str()).unwrap_or(&path),
                        mtime.as_secs()
                    ),
                );
            }
        }
        bar.finish_and_clear();
        if skipped > 0 {
            eprintln!("[baztet] {skipped} aspect output(s) skipped (no target key)");
        }
    });

    // prune reuse metadata whose BEP keys no longer exist in this build
    let live: BTreeSet<String> = output
        .target_outputs
        .keys()
        .map(|k| format!("{}!{}", k.label, k.configuration))
        .collect();
    snapshot.ide_sources_meta.retain(|k, _| live.contains(k));

    // prune ide summaries for targets that no longer exist in the graph
    // (otherwise deleted targets linger forever in stats/targets output)
    snapshot.ide_infos.retain(|key, _| {
        snapshot
            .graph
            .get_by_label(key.split('!').next().unwrap_or(""))
            .is_some()
    });
}

fn parse_count() -> usize {
    PARSE_COUNT.load(std::sync::atomic::Ordering::Relaxed)
}

fn aspect_output_mtime(path: &camino::Utf8PathBuf) -> Option<std::time::Duration> {
    std::fs::metadata(path)
        .ok()
        .and_then(|m| m.modified().ok())
        .and_then(|m| m.duration_since(std::time::UNIX_EPOCH).ok())
}

/// The recorded meta is `relpath@mtime_secs`; an output counts as unchanged
/// only when both match exactly (bazel rewrites aspect files when the
/// analysis output changes).
fn file_is_newer(
    mtime: Option<std::time::Duration>,
    relative: &str,
    meta: Option<&String>,
) -> bool {
    let (Some(mtime), Some(meta)) = (mtime, meta) else {
        return true;
    };
    meta != &format!("{relative}@{}", mtime.as_secs())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn aspect_output_meta_contract() {
        // unchanged path+mtime -> already fresh (not "newer"); anything else re-parses
        assert!(!file_is_newer(
            Some(std::time::Duration::new(1_700_000_000, 0)),
            "bazel-out/x/f.txt",
            Some(&"bazel-out/x/f.txt@1700000000".to_string())
        ));
        assert!(file_is_newer(
            Some(std::time::Duration::new(1_700_000_001, 0)),
            "bazel-out/x/f.txt",
            Some(&"bazel-out/x/f.txt@1700000000".to_string())
        ));
        assert!(file_is_newer(
            Some(std::time::Duration::new(1, 0)),
            "bazel-out/x/CHANGED.txt",
            Some(&"bazel-out/x/OLD.txt@1700000000".to_string())
        ));
        // no meta (first sync) -> newer
        assert!(file_is_newer(Some(std::time::Duration::new(1, 0)), "a", None));
        assert!(file_is_newer(None, "a", Some(&"a@1".to_string())));
    }

    #[test]
    fn fingerprint_sensitive_to_runner() {
        let pv = ProjectView::default();
        let specs = vec!["//:all".to_string()];
        let mut info = crate::workspace::BazelInfo::default();
        info.release = "release 9.2.0".into();
        let r1 = BazelRunner {
            binary: "bazel".into(),
            workspace_root: "/ws".into(),
            info: info.clone(),
        };
        info.release = "release 9.3.0".into();
        let r2 = BazelRunner {
            binary: "bazel".into(),
            workspace_root: "/ws".into(),
            info,
        };
        assert_eq!(sync_fingerprint(&specs, &pv, &r1), sync_fingerprint(&specs, &pv, &r1));
        assert_ne!(sync_fingerprint(&specs, &pv, &r1), sync_fingerprint(&specs, &pv, &r2));
    }
}