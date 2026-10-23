//! Sync pipeline: query phase (see [`crate::query`]) + aspect/BEP phase, the
//! incremental reuse ladder (see [`crate::reuse`]), and snapshot merging.

use std::collections::{BTreeMap, BTreeSet};

use baztet_bep::BepOutput;
use baztet_projectview::ProjectView;
use baztet_snapshot::Snapshot;

use crate::aspect::{self, AspectConfig, Rules};
use crate::bazel::{BazelError, BazelRunner};
use crate::expand::package_build_mtimes;
pub use crate::query::{batch_specs, target_specs, DEFAULT_SHARD_SIZE};
pub use crate::reuse::{decide_reuse, sync_fingerprint, CurrentFacts, RecordedFacts, Reuse};

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
    let recorded_pkg_mtimes = existing.as_ref().and_then(|s| s.build_pkg_mtimes.clone());

    // one walk feeds the ladder, the delta selector and the recording
    let current_mtimes = package_build_mtimes(pv, runner.workspace_root.as_str());
    let current = CurrentFacts {
        build_mtime: current_mtimes.values().max().copied(),
        pkg_mtimes: current_mtimes.clone(),
        directories_configured: !pv.directories.entries.is_empty(),
    };

    if let Some(existing) = existing {
        let specs = target_specs(pv, runner.workspace_root.as_str());
        let fingerprints_match = existing.sync_fingerprint
            == Some(sync_fingerprint(&specs, pv, runner))
            && !existing.graph.is_empty();
        let recorded = RecordedFacts {
            build_mtime: existing.build_files_mtime,
            pkg_mtimes: recorded_pkg_mtimes.clone(),
        };
        match decide_reuse(
            fingerprints_match,
            &recorded,
            &current,
            run_aspect_phase,
            !existing.ide_infos.is_empty(),
        ) {
            Reuse::Everything => {
                let mut snapshot = existing;
                refresh_snapshot_meta(&mut snapshot, runner, &current_mtimes);
                return Ok(crate::reuse::SyncReport {
                    snapshot,
                    reuse: Reuse::Everything,
                    query_secs: Some(t0.elapsed().as_secs_f64()),
                    aspect_secs: None,
                    parsed: 0,
                });
            }
            Reuse::Graph => {
                eprintln!("[baztet] project view unchanged; refreshing aspect outputs");
                let mut snapshot = existing;
                let all_targets = snapshot.specified_targets.clone();
                // refresh starts clean; a failing aspect re-marks it
                snapshot.has_error = false;
                let aspect_secs =
                    run_aspect_phase_once(runner, pv, &mut snapshot, &all_targets, false).await;
                refresh_snapshot_meta(&mut snapshot, runner, &current_mtimes);
                return Ok(crate::reuse::SyncReport {
                    snapshot,
                    reuse: Reuse::Graph,
                    // no query ran; report 0 elapsed for phase 1, and the
                    // measured aspect window for phase 2
                    query_secs: Some(0.0),
                    aspect_secs: Some(aspect_secs),
                    parsed: parse_count(),
                });
            }
            Reuse::FullQuery => {}
        }
    }
    let specs = target_specs(pv, runner.workspace_root.as_str());

    // full path: query, then optional aspect phase
    let (graph, query_failed) = crate::query::query_graph(runner, pv, &specs).await?;
    let query_secs = t0.elapsed().as_secs_f64();
    let mut snapshot = Snapshot::new(runner.workspace_root.clone());
    snapshot.specified_targets = specs;
    snapshot.graph = graph;
    snapshot.has_error = query_failed;
    snapshot.sync_fingerprint = Some(sync_fingerprint(&snapshot.specified_targets, pv, runner));
    snapshot.bazel_version = runner.info.bazel_version().map(str::to_owned);

    let mut aspect_secs = None;
    let mut parsed = 0usize;
    if run_aspect_phase {
        let (targets, delta_ran) = delta_aspect_targets(
            recorded_pkg_mtimes.as_ref(),
            &current_mtimes,
            &snapshot.specified_targets,
        );
        aspect_secs =
            Some(run_aspect_phase_once(runner, pv, &mut snapshot, &targets, delta_ran).await);
        parsed = parse_count();
    }
    refresh_snapshot_meta(&mut snapshot, runner, &current_mtimes);
    Ok(crate::reuse::SyncReport {
        reuse: Reuse::FullQuery,
        snapshot,
        query_secs: Some(query_secs),
        aspect_secs,
        parsed,
    })
}

/// Record this run's walker facts and the live `bazel info` text; every
/// ladder outcome uses this so no path persists stale cache keys.
fn refresh_snapshot_meta(
    snapshot: &mut Snapshot,
    runner: &BazelRunner,
    current_mtimes: &BTreeMap<String, u64>,
) {
    snapshot.build_files_mtime = current_mtimes.values().max().copied();
    snapshot.build_pkg_mtimes = (!current_mtimes.is_empty()).then(|| current_mtimes.clone());
    snapshot.bazel_info_text = runner.info.text.clone();
    snapshot.bazel_binary = Some(runner.binary.clone());
}

/// Aspect-build targets. Delta mode selects only labels in packages whose
/// BUILD changed; no recorded map, no changes, or no label matched falls
/// back to every target — a narrower list would leave outputs missing.
fn delta_aspect_targets(
    recorded: Option<&BTreeMap<String, u64>>,
    current_mtimes: &BTreeMap<String, u64>,
    specs: &[String],
) -> (Vec<String>, bool) {
    let Some(recorded) = recorded.filter(|r| !r.is_empty() && !specs.is_empty()) else {
        return (specs.to_vec(), false);
    };
    let changed = changed_packages(recorded, current_mtimes);
    let targets: Vec<String> = changed
        .iter()
        .flat_map(|pkg| {
            let prefix = format!("//{pkg}:");
            specs
                .iter()
                .filter(move |s| s == &pkg || s.starts_with(&prefix))
                .cloned()
        })
        .collect();
    if targets.is_empty() {
        return (specs.to_vec(), false);
    }
    (targets, true)
}

/// Packages whose BUILD mtime changed or that are new vs the recorded map.
/// Removed packages don't matter: their targets left the graph already.
fn changed_packages(
    recorded: &BTreeMap<String, u64>,
    current: &BTreeMap<String, u64>,
) -> BTreeSet<String> {
    current
        .iter()
        .filter(|(pkg, mtime)| recorded.get(*pkg) != Some(*mtime))
        .map(|(pkg, _)| pkg.clone())
        .collect()
}

/// Run the aspect phase and persist its error into the snapshot; returns seconds.
async fn run_aspect_phase_once(
    runner: &BazelRunner,
    pv: &ProjectView,
    snapshot: &mut Snapshot,
    targets: &[String],
    delta_ran: bool,
) -> f64 {
    let t = std::time::Instant::now();
    match aspect_sync(runner, pv, snapshot, targets, delta_ran).await {
        Ok(()) => {}
        Err(e) => {
            eprintln!("[baztet] aspect sync failed: {e}");
            snapshot.has_error = true;
        }
    }
    t.elapsed().as_secs_f64()
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

/// Phase 2: aspect sync → TargetIdeInfo summaries. `targets` is the build
/// target list (delta subset or all); `delta_ran` marks a scoped BEP, whose
/// entries say nothing about the untouched targets' outputs.
pub async fn aspect_sync(
    runner: &BazelRunner,
    pv: &ProjectView,
    snapshot: &mut Snapshot,
    targets: &[String],
    delta_ran: bool,
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
    argv.extend(targets.iter().cloned());
    argv.extend(pv.sync_flags.iter().cloned());

    let (code, _stdout, stderr) = runner.run_simple("build", &argv).await?;
    let bytes = std::fs::read(&bep_file)
        .map_err(|e| BazelError::Other(format!("failed to read BEP file {bep_file:?}: {e}")))?;
    let _ = std::fs::remove_file(&bep_file);

    let output = baztet_bep::parse(&bytes)
        .map_err(|e| BazelError::Other(format!("failed to parse BEP: {e}")))?;

    // prefer `bazel config --dump_all` (bzlmod config ids); BEP events as fallback
    let fetched = fetch_configurations(runner, &output).await;
    if delta_ran {
        // a delta BEP only covers the changed subset; merge, don't replace,
        // or configurations recorded for untouched targets would vanish
        snapshot.configurations.extend(fetched);
    } else {
        snapshot.configurations = fetched;
    }

    merge_ide_infos(runner, &output, snapshot, delta_ran);

    if code != 0 {
        eprintln!("[baztet] aspect build exited with code {code}:\n{stderr}");
        snapshot.has_error = true;
    }
    Ok(())
}

/// Configurations from BEP `BuildConfiguration` events; the
/// `bazel config --dump_all` fallback stays behind this function until a
/// CLI flag consumes configurations.
async fn fetch_configurations(
    _runner: &BazelRunner,
    output: &BepOutput,
) -> std::collections::BTreeMap<String, baztet_snapshot::ConfigurationLite> {
    crate::ideinfo::configurations_from_bep(&output.configurations)
}

/// Parse count set by the rayon workers, read by `sync` for SyncReport.parsed.
static PARSE_COUNT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

fn merge_ide_infos(
    runner: &BazelRunner,
    output: &BepOutput,
    snapshot: &mut Snapshot,
    delta_ran: bool,
) {
    use rayon::prelude::*;

    PARSE_COUNT.store(0, std::sync::atomic::Ordering::Relaxed);
    let execution_root = runner.info.execution_root.clone();

    // work items: (BEP key, file path, mtime)
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

    // rayon blocks the calling worker; block_in_place keeps this executor
    // responsive. Panics on a single-thread runtime, which the CLI never uses.
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

        // merge sequentially; entries without a target key are skipped
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

    // delta BEPs only cover the changed subset: skip the prune there, the
    // untouched outputs stay valid (lingering stale keys cost nothing)
    if !delta_ran {
        let live: BTreeSet<String> = output
            .target_outputs
            .keys()
            .map(|k| format!("{}!{}", k.label, k.configuration))
            .collect();
        snapshot.ide_sources_meta.retain(|k, _| live.contains(k));
    }

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

/// The recorded meta is `relpath@mtime_secs`; bazel rewrites aspect files
/// when the analysis output changes, so both parts must match exactly.
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
    fn delta_packages_are_changed_and_new() {
        let recorded: BTreeMap<String, u64> =
            BTreeMap::from([("a".into(), 1), ("b".into(), 2), ("gone".into(), 3)]);
        let current: BTreeMap<String, u64> = BTreeMap::from([
            ("a".into(), 1),   // unchanged
            ("b".into(), 5),   // changed
            ("new".into(), 7), // added
        ]);
        let delta = changed_packages(&recorded, &current);
        assert_eq!(delta, BTreeSet::from(["b".to_string(), "new".to_string()]));
    }

    #[test]
    fn no_delta_when_identical() {
        let m: BTreeMap<String, u64> = BTreeMap::from([("a".into(), 1)]);
        assert!(changed_packages(&m, &m).is_empty());
    }

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
        assert!(file_is_newer(
            Some(std::time::Duration::new(1, 0)),
            "a",
            None
        ));
        assert!(file_is_newer(None, "a", Some(&"a@1".to_string())));
    }

    #[test]
    fn fingerprint_sensitive_to_runner() {
        let pv = ProjectView::default();
        let specs = vec!["//:all".to_string()];
        let runner = |release: &str| BazelRunner {
            binary: "bazel".into(),
            workspace_root: "/ws".into(),
            info: crate::workspace::BazelInfo {
                release: release.into(),
                ..Default::default()
            },
            tuning_flags: Vec::new(),
        };
        let r1 = runner("release 9.2.0");
        let r2 = runner("release 9.3.0");
        assert_eq!(
            sync_fingerprint(&specs, &pv, &r1),
            sync_fingerprint(&specs, &pv, &r1)
        );
        assert_ne!(
            sync_fingerprint(&specs, &pv, &r1),
            sync_fingerprint(&specs, &pv, &r2)
        );
    }
}
