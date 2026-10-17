//! Phase-1 (query) sync: `bazel query --output=streamed_proto` → TargetGraph.
//!
//! Mirrors hirschgarten's `FirstPhaseProjectResolver`: query the project view
//! targets with `--keep_going`, stream-decode `Build.Target` messages, build
//! the target graph.

use baztet_projectview::ProjectView;
use baztet_snapshot::{Snapshot, TargetGraph};
use baztet_protos::blaze_query;

use crate::bazel::{BazelError, BazelRunner};

/// Target specs passed to `bazel query`, in project-view order.
pub fn target_specs(pv: &ProjectView) -> Vec<String> {
    let mut specs = pv.targets.includes.clone();
    if pv.derive_targets_from_directories() {
        for entry in &pv.directories.entries {
            if !entry.exclude {
                specs.push(entry.as_target());
            }
        }
    }
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

/// Build the `bazel query` expression for the synced specs.
pub fn query_expression(specs: &[String]) -> String {
    if specs.len() == 1 {
        specs[0].clone()
    } else {
        format!("set({})", specs.join(" "))
    }
}

/// Run phase-1 sync and build the query snapshot.
pub async fn query_sync(runner: &BazelRunner, pv: &ProjectView) -> Result<Snapshot, BazelError> {
    let specs = target_specs(pv);
    let argv = vec![
        "--output=streamed_proto".to_string(),
        "--keep_going".to_string(),
        query_expression(&specs),
    ];

    let (targets, code, stderr) =
        runner.run_streaming_protos::<blaze_query::Target>("query", &argv).await?;

    let mut graph = TargetGraph::default();

    // pass 1: create nodes for returned rules
    let mut rules: Vec<(u32, &blaze_query::Rule)> = Vec::new();
    for target in &targets {
        if target.r#type == blaze_query::target::Discriminator::Rule as i32 {
            if let Some(rule) = &target.rule {
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