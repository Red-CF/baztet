//! Sync pipeline: phase 1 (streamed query) and phase 2 (aspect + BEP + TargetIdeInfo parse), with the incremental reuse ladder.

use std::collections::{BTreeMap, BTreeSet};

use baztet_bep::BepOutput;
use baztet_projectview::ProjectView;
use baztet_protos::blaze_query;
use baztet_snapshot::{Snapshot, TargetGraph};

use camino::Utf8PathBuf;

use crate::aspect::{self, AspectConfig, Rules};
use crate::bazel::{BazelError, BazelRunner};
use crate::expand::{expand_directory_targets, label_is_excluded};

/// Query specs: project view `targets:` + derived `directories:` packages.
pub fn target_specs(pv: &ProjectView, workspace_root: &str) -> Vec<String> {
    let mut specs = pv.targets.includes.clone();
    specs.extend(expand_directory_targets(pv, workspace_root).0);
    // dedupe, keep order
    let mut seen = std::collections::HashSet::new();
    specs.retain(|s| seen.insert(s.clone()));
    specs
}

fn rule_srcs(rule: &blaze_query::Rule) -> Vec<String> {
    rule.attribute
        .iter()
        .filter(|a| a.name == "srcs" && !a.nodep.unwrap_or(false))
        .flat_map(|a| a.string_list_value.iter().cloned())
        .collect()
}

fn query_expression(specs: &[String]) -> String {
    if specs.len() == 1 {
        specs[0].clone()
    } else {
        format!("set({})", specs.join(" "))
    }
}

/// Full sync; `run_aspect_phase=false` stops after the query phase, `force_full` skips the reuse ladder.
/// What one `sync` run did: reuse level, timings, parse counts.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct SyncReport {
    pub snapshot: Option<Snapshot>,
    /// how the query graph came about
    pub reuse: Reuse,
    /// phase timings (seconds), when the phase ran
    pub query_secs: Option<f64>,
    pub aspect_secs: Option<f64>,
    /// aspect outputs parsed fresh this run (rayon-parallel)
    pub parsed: usize,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Reuse {
    #[default]
    FullQuery,
    /// same project view -> graph reused, aspect outputs refreshed
    Graph,
    /// same project view AND unchanged BUILD files -> whole snapshot reused
    Everything,
}

pub async fn sync(
    runner: &BazelRunner,
    pv: &ProjectView,
    run_aspect_phase: bool,
    force_full: bool,
) -> Result<SyncReport, BazelError> {
    let t0 = std::time::Instant::now();

    let specs = target_specs(pv, runner.workspace_root.as_str());

    if !force_full {
        if let Ok(existing) = Snapshot::load(&runner.workspace_root.clone()) {
            if existing.sync_fingerprint == Some(sync_fingerprint(&specs, pv, runner))
                && !existing.graph.is_empty()
            {
                let builds_unchanged = match (
                    existing.build_files_mtime,
                    max_build_mtime(pv, &runner.workspace_root),
                ) {
                    (Some(recorded), Some(current)) => current <= recorded,
                    // no directories to derive from -> nothing can have drifted via the walker
                    (r, None) => r.is_some() || pv.directories.entries.is_empty(),
                    (None, Some(_)) => false,
                };
                // stale BUILDs mean a NEW target list -> the graph and the
                // aspect spec list must be rebuilt; reuse only when nothing changed
                if builds_unchanged {
                    if !run_aspect_phase || !existing.ide_infos.is_empty() {
                        return Ok(SyncReport {
                            snapshot: Some(existing),
                            reuse: Reuse::Everything,
                            query_secs: Some(t0.elapsed().as_secs_f64()),
                            ..Default::default()
                        });
                    }
                    eprintln!("[baztet] project view unchanged; refreshing aspect outputs");
                    let mut snapshot = existing;
                    match aspect_sync(runner, pv, &mut snapshot).await {
                        Ok(()) => {}
                        Err(e) => {
                            eprintln!("[baztet] aspect sync failed: {e}");
                            snapshot.has_error = true;
                        }
                    }
                    return Ok(SyncReport {
                        snapshot: Some(snapshot),
                        reuse: Reuse::Graph,
                        query_secs: Some(t0.elapsed().as_secs_f64()),
                        aspect_secs: Some(t0.elapsed().as_secs_f64()),
                        ..Default::default()
                    });
                }
                eprintln!("[baztet] BUILD files changed; re-querying the graph");
            }
        }
    }

    let mut snapshot = query_sync(runner, pv).await?;
    let query_secs = t0.elapsed().as_secs_f64();
    let mut aspect_secs = None;
    if run_aspect_phase {
        let phase_start = std::time::Instant::now();
        match aspect_sync(runner, pv, &mut snapshot).await {
            Ok(()) => {}
            Err(e) => {
                eprintln!("[baztet] aspect sync failed: {e}");
                snapshot.has_error = true;
            }
        }
        aspect_secs = Some(phase_start.elapsed().as_secs_f64());
    }
    snapshot.build_files_mtime = max_build_mtime(pv, &runner.workspace_root);
    Ok(SyncReport {
        snapshot: Some(snapshot),
        reuse: Reuse::FullQuery,
        query_secs: Some(query_secs),
        aspect_secs,
        ..Default::default()
    })
}

/// Max BUILD mtime from the package walk (None when derive is off).
fn max_build_mtime(pv: &ProjectView, workspace_root: &Utf8PathBuf) -> Option<u64> {
    expand_directory_targets(pv, workspace_root.as_str()).1
}

/// Stable fingerprint of everything that changes the query result.
pub fn sync_fingerprint(specs: &[String], pv: &ProjectView, runner: &BazelRunner) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    specs.hash(&mut hasher);
    pv.targets.excludes.hash(&mut hasher);
    pv.build_flags.hash(&mut hasher);
    pv.sync_flags.hash(&mut hasher);
    format!("{:?}", (pv.shard_sync(), pv.target_shard_size())).hash(&mut hasher);
    runner.info.bazel_version().unwrap_or("").hash(&mut hasher);
    hasher.finish()
}

/// Phase 1: query sync → target graph (with batching + OOM halving).
pub async fn query_sync(runner: &BazelRunner, pv: &ProjectView) -> Result<Snapshot, BazelError> {
    let ws_root = runner.workspace_root.clone();
    let specs = target_specs(pv, ws_root.as_str());

    let mut graph = TargetGraph::default();
    let mut batches = batch_specs(&specs, pv);
    let mut has_error = false;
    while let Some(batch) = batches.pop_front() {
        match query_batch(runner, &batch).await {
            Ok((targets, code, stderr)) => {
                if code != 0 {
                    has_error = true;
                    eprintln!("[baztet] query exited with code {code}:\n{stderr}");
                }
                build_graph_from_targets(&mut graph, &targets, &pv.targets.excludes);
            }
            Err(e) => return Err(e),
        }
    }

    let mut snapshot = Snapshot::new(runner.workspace_root.clone());
    snapshot.specified_targets = specs;
    snapshot.graph = graph;
    snapshot.has_error = has_error;
    snapshot.sync_fingerprint = Some(sync_fingerprint(&snapshot.specified_targets, pv, runner));
    snapshot.bazel_version = runner.info.bazel_version().map(str::to_owned);
    Ok(snapshot)
}

/// Graph nodes/edges from one batch of query results.
fn build_graph_from_targets(
    graph: &mut TargetGraph,
    targets: &[blaze_query::Target],
    excludes: &[String],
) {
    let mut rules: Vec<(u32, &blaze_query::Rule)> = Vec::new();
    for target in targets {
        if target.r#type == blaze_query::target::Discriminator::Rule as i32 {
            if let Some(rule) = &target.rule {
                if label_is_excluded(&rule.name, excludes) {
                    continue;
                }
                let srcs = rule_srcs(rule);
                let idx = graph.get_or_insert(
                    rule.name.clone(),
                    rule.rule_class.clone(),
                    rule.location.clone(),
                    srcs,
                );
                rules.push((idx, rule));
            }
        }
    }

    for (idx, rule) in rules {
        for input in &rule.rule_input {
            match graph.index_of(input) {
                Some(dep) => graph.add_dep(idx, dep),
                None => graph.add_unlinked_dep(idx, input.clone()),
            }
        }
    }
}

/// Lexicographic batching (shard_sync + target_shard_size); mirrors the plugin's batcher.
pub const DEFAULT_SHARD_SIZE: usize = 2_000;

pub fn batch_specs(specs: &[String], pv: &ProjectView) -> std::collections::VecDeque<Vec<String>> {
    if !pv.shard_sync() || specs.len() <= 1 {
        return std::iter::once(specs.to_vec()).collect();
    }
    let shard_size = pv
        .target_shard_size()
        .map(|s| s as usize)
        .unwrap_or(DEFAULT_SHARD_SIZE);
    let mut sorted: Vec<String> = specs.to_vec();
    sorted.sort();
    sorted
        .chunks(shard_size.max(1))
        .map(<[String]>::to_vec)
        .collect()
}

/// One query batch; on heap OOM split in half and query BOTH halves.
type BatchResult = Result<(Vec<blaze_query::Target>, i32, String), BazelError>;
type BatchFuture<'a> =
    std::pin::Pin<Box<dyn std::future::Future<Output = BatchResult> + Send + 'a>>;

fn query_batch<'a>(runner: &'a BazelRunner, batch: &'a [String]) -> BatchFuture<'a> {
    Box::pin(query_batch_impl(runner, batch))
}

async fn query_batch_impl(
    runner: &BazelRunner,
    batch: &[String],
) -> Result<(Vec<blaze_query::Target>, i32, String), BazelError> {
    let argv = vec![
        "--output=streamed_proto".to_string(),
        "--keep_going".to_string(),
        query_expression(batch),
    ];
    let (targets, code, stderr) = runner
        .run_streaming_protos::<blaze_query::Target>("query", &argv)
        .await?;

    let oom = code != 0
        && (stderr.to_ascii_lowercase().contains("java heap")
            || stderr.to_ascii_lowercase().contains("out of memory"));
    if !oom || batch.len() <= 1 {
        return Ok((targets, code, stderr));
    }

    eprintln!(
        "[baztet] query shard OOM ({} specs); splitting batch",
        batch.len()
    );
    let mid = batch.len() / 2;
    let (mut first, c1, e1) = query_batch(runner, &batch[..mid]).await?;
    let (second, c2, e2) = query_batch(runner, &batch[mid..]).await?;
    first.extend(second);
    let code = if c1 != 0 { c1 } else { c2 };
    Ok((first, code, format!("{e1}{e2}")))
}

/// Where the aspect gets deployed (relative to the workspace root).
pub const ASPECT_PREFIX: &str = ".baztet/aspect";

/// Rule kinds the aspect modules care about, by language.
pub fn detect_languages(graph: &TargetGraph) -> BTreeSet<Rules> {
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
    let mapping = bazel_repo_mapping(runner);
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

/// Read `*.intellij-info.txt` files from the BEP output groups and merge
/// their summaries into the snapshot. Unchanged aspect outputs (same path
/// and mtime as the last sync) are reused from the existing snapshot;
/// changed ones parse in parallel (rayon) — this is the bulk of baztet's
/// cold-sync CPU.
fn merge_ide_infos(runner: &BazelRunner, output: &BepOutput, snapshot: &mut Snapshot) {
    use rayon::prelude::*;

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

    // 2) parse in parallel (indicatif progress is Send, rayon drives the loop)
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

    // prune reuse metadata whose BEP keys no longer exist in this build
    let live: BTreeSet<String> = output
        .target_outputs
        .keys()
        .map(|k| format!("{}!{}", k.label, k.configuration))
        .collect();
    snapshot.ide_sources_meta.retain(|k, _| live.contains(k));
}
fn aspect_output_mtime(path: &camino::Utf8PathBuf) -> Option<std::time::Duration> {
    std::fs::metadata(path).ok().map(|m| {
        m.modified()
            .expect("file exists but no mtime")
            .duration_since(std::time::UNIX_EPOCH)
            .expect("mtime before epoch")
    })
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
    use crate::workspace::BazelInfo;
    use blaze_query::target::Discriminator;
    use blaze_query::Attribute;

    fn rule_target(
        name: &str,
        inputs: &[&str],
        srcs: &[&str],
        excluded: bool,
    ) -> blaze_query::Target {
        let mut r = blaze_query::Rule {
            name: name.into(),
            rule_class: "java_library".into(),
            location: Some("//a/BUILD:1".into()),
            ..Default::default()
        };
        r.rule_input = inputs.iter().map(|s| s.to_string()).collect();
        r.attribute = vec![Attribute {
            name: "srcs".into(),
            string_list_value: srcs.iter().map(|s| s.to_string()).collect(),
            ..Default::default()
        }];
        let _ = excluded; // excludes are filtered by name upstream of the graph
        blaze_query::Target {
            r#type: Discriminator::Rule as i32,
            rule: Some(r),
            ..Default::default()
        }
    }

    fn discriminator_of(t: &blaze_query::Target) -> i32 {
        t.r#type
    }

    #[test]
    fn graph_build_links_deps_and_unlinked() {
        let mut graph = TargetGraph::default();
        let rules = vec![
            rule_target(
                "//src:a",
                &["//src:b", "@maven//:ext"],
                &["src/A.java"],
                false,
            ),
            rule_target("//src:b", &[], &["src/B.java"], false),
        ];
        build_graph_from_targets(&mut graph, &rules, &[]);
        assert_eq!(graph.len(), 2);
        let a = graph.index_of("//src:a").unwrap();
        let b = graph.index_of("//src:b").unwrap();
        assert_eq!(graph.get_by_index(a).unwrap().deps, [b]);
        assert_eq!(graph.get_by_index(a).unwrap().srcs, ["src/A.java"]);
        // external input stays unlinked (not a node)
        assert_eq!(graph.unlinked_deps(a), &["@maven//:ext".to_string()]);
        let _ = discriminator_of(&rules[0]);
    }

    #[test]
    fn graph_build_honours_excludes() {
        let mut graph = TargetGraph::default();
        build_graph_from_targets(
            &mut graph,
            &[rule_target("//gen:skip", &[], &[], false)],
            &["-//gen/...".to_string()],
        );
        assert!(graph.is_empty());
        assert!(graph.get_by_label("//gen:skip").is_none());
    }

    fn pv(sizes: &[(bool, Option<u32>)]) -> ProjectView {
        let (shard, size) = sizes[0];
        let _pv = ProjectView::default();
        // shard_sync and target_shard_size are set via parse_text for parity
        let mut text = format!("shard_sync: {}", if shard { "true" } else { "false" });
        if let Some(v) = size {
            text.push_str(&format!("\ntarget_shard_size: {v}"));
        }
        baztet_projectview::parse_text(&text).unwrap()
    }

    #[test]
    fn batching_defaults() {
        let specs: Vec<String> = (0..10).map(|i| format!("//p{i}:all")).collect();
        let no_shard = pv(&[(false, None)]);
        let mut batches = batch_specs(&specs, &no_shard);
        assert_eq!(batches.len(), 1);
        assert_eq!(batches.pop_front().unwrap().len(), 10);

        let shard = pv(&[(true, Some(3))]);
        let batches = batch_specs(&specs, &shard);
        assert_eq!(batches.len(), 4); // 3+3+3+1
                                      // lexicographic order
        let first = batches.front().unwrap();
        assert_eq!(
            first,
            &[
                "//p0:all".to_string(),
                "//p1:all".to_string(),
                "//p2:all".to_string()
            ]
        );
    }

    #[test]
    fn single_spec_no_batching() {
        let shard = pv(&[(true, Some(2))]);
        let batches = batch_specs(&["//only:all".to_string()], &shard);
        assert_eq!(batches.len(), 1);
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
    fn fingerprint_stable_and_sensitive() {
        let mut pv = ProjectView::default();
        let specs = vec!["//:all".to_string()];
        let info = BazelInfo {
            release: "release 9.2.0".into(),
            ..Default::default()
        };
        let f1 = sync_fingerprint(&specs, &pv, &fake(&info));
        let f2 = sync_fingerprint(&specs, &pv, &fake(&info));
        assert_eq!(f1, f2);
        pv.build_flags.push("--x".into());
        let f3 = sync_fingerprint(&specs, &pv, &fake(&info));
        assert_ne!(f1, f3);
    }

    fn fake(info: &BazelInfo) -> BazelRunner {
        BazelRunner {
            binary: "bazel".into(),
            workspace_root: "/ws".into(),
            info: info.clone(),
        }
    }
}
