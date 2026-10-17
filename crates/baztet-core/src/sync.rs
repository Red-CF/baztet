//! Sync pipeline, mirroring hirschgarten's two phases:
//!
//! - phase 1 (query): `bazel query --output=streamed_proto` → target graph
//!   (port of `FirstPhaseProjectResolver`)
//! - phase 2 (aspect): deploy the vendored intellij-aspect, run
//!   `bazel build --aspects` with a BEP sidecar file, parse `TargetIdeInfo`
//!   outputs (port of `ProjectResolver.buildProjectWithAspect*`)

use std::collections::{BTreeMap, BTreeSet};

use baztet_bep::{BepFile, BepOutput};
use baztet_projectview::ProjectView;
use baztet_snapshot::{Snapshot, TargetGraph};
use baztet_protos::blaze_query;

use crate::aspect::{self, AspectConfig, Rules};
use crate::bazel::{BazelError, BazelRunner};
use crate::expand::{expand_directory_targets, label_is_excluded};

/// Target specs passed to `bazel query`, in project-view order: explicit
/// `targets:` entries (bazel query resolves wildcards) plus every package
/// found under the `directories:` entries when
/// `derive_targets_from_directories` is enabled.
pub fn target_specs(pv: &ProjectView, workspace_root: &str) -> Vec<String> {
    let mut specs = pv.targets.includes.clone();
    specs.extend(expand_directory_targets(pv, workspace_root));
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

/// Run the full sync (phase 1 + phase 2). `phase2` false stops after the
/// query phase.
pub async fn sync(
    runner: &BazelRunner,
    pv: &ProjectView,
    run_aspect_phase: bool,
) -> Result<Snapshot, BazelError> {
    let mut snapshot = query_sync(runner, pv).await?;
    if run_aspect_phase {
        match aspect_sync(runner, pv, &mut snapshot).await {
            Ok(()) => {}
            Err(e) => {
                eprintln!("[baztet] aspect sync failed: {e}");
                snapshot.has_error = true;
            }
        }
    }
    Ok(snapshot)
}

/// Phase 1: query sync → target graph.
pub async fn query_sync(runner: &BazelRunner, pv: &ProjectView) -> Result<Snapshot, BazelError> {
    let ws_root = runner.workspace_root.clone();
    let specs = target_specs(pv, ws_root.as_str());
    let argv = vec![
        "--output=streamed_proto".to_string(),
        "--keep_going".to_string(),
        query_expression(&specs),
    ];

    let (targets, code, stderr) =
        runner.run_streaming_protos::<blaze_query::Target>("query", &argv).await?;

    let mut graph = TargetGraph::default();

    // pass 1: create nodes for returned rules, minus target excludes
    let mut rules: Vec<(u32, &blaze_query::Rule)> = Vec::new();
    for target in &targets {
        if target.r#type == blaze_query::target::Discriminator::Rule as i32 {
            if let Some(rule) = &target.rule {
                if label_is_excluded(&rule.name, &pv.targets.excludes) {
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
        // SourceFile / GeneratedFile targets are covered by rule srcs
        // attributes; they do not become graph nodes.
    }

    // pass 2: wire deps; inputs outside the synced set stay unlinked
    for (idx, rule) in rules {
        for input in &rule.rule_input {
            match graph.index_of(input) {
                Some(dep) => graph.add_dep(idx, dep),
                None => graph.add_unlinked_dep(idx, input.clone()),
            }
        }
    }

    let mut snapshot = Snapshot::new(runner.workspace_root.clone());
    snapshot.specified_targets = specs;
    snapshot.graph = graph;
    snapshot.has_error = code != 0;
    snapshot.bazel_version = runner.info.bazel_version().map(str::to_owned);
    if snapshot.has_error {
        eprintln!("[baztet] query exited with code {code}:\n{stderr}");
    }
    Ok(snapshot)
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

/// Canonical repo mapping via bzlmod (`bazel mod dump_repo_mapping ""`):
/// default repo name -> canonical repo name.
fn bazel_repo_mapping(runner: &BazelRunner) -> BTreeMap<String, String> {
    let (code, stdout, stderr) = match runner.run_simple_blocking("mod", &["dump_repo_mapping".into(), "".into()]) {
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
pub async fn aspect_sync(runner: &BazelRunner, pv: &ProjectView, snapshot: &mut Snapshot) -> Result<(), BazelError> {
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
    for rule in [Rules::Kotlin, Rules::Scala, Rules::Go, Rules::Python, Rules::Cc] {
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

    snapshot.configurations = crate::ideinfo::configurations_from_bep(&output.configurations);
    merge_ide_infos(runner, &output, snapshot);

    if code != 0 {
        eprintln!("[baztet] aspect build exited with code {code}:\n{stderr}");
        snapshot.has_error = true;
    }
    Ok(())
}

/// Read `*.intellij-info.txt` files from the BEP output groups and merge
/// their summaries into the snapshot.
fn merge_ide_infos(runner: &BazelRunner, output: &BepOutput, snapshot: &mut Snapshot) {
    let execution_root = runner.info.execution_root.clone();
    for (key, _groups) in &output.target_outputs {
        let (label, configuration) = (&key.label, &key.configuration);
        let files = output.files_for_target_group(&key, baztet_bep::OUTPUT_GROUP_INFO);
        for file in &files {
            let Some(text) = read_aspect_output(&execution_root, file) else { continue };
            let parsed = match crate::ideinfo::parse(&text) {
                Ok(m) => m,
                Err(e) => {
                    eprintln!("[baztet] skipping unparsable aspect output {label} ({file:?}): {e}");
                    continue;
                }
            };
            let (identity, mut summary) = crate::ideinfo::summarize(&parsed);
            let Some((ilabel, iconfig)) = identity else { continue };
            // prefer the summary's own identity; fall back to the BEP key
            let key_label = if ilabel.is_empty() { label.clone() } else { ilabel };
            let key_config = if iconfig.is_empty() { configuration.clone() } else { iconfig };
            // keep kind from the query phase
            if let Some(node) = snapshot.graph.get_by_label(&key_label) {
                summary.kind = node.kind.clone();
            }
            snapshot
                .ide_infos
                .insert(format!("{key_label}!{key_config}"), summary);
        }
    }
}

fn read_aspect_output(execution_root: &camino::Utf8PathBuf, file: &BepFile) -> Option<String> {
    let relative = file.relative_path();
    let path = execution_root.join(&relative);
    std::fs::read_to_string(path).ok()
}