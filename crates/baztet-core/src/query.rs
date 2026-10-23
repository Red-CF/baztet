//! Phase 1: `bazel query` -> target graph, with lexicographic batching and
//! OOM halving (mirrors the plugin's sharder).

use std::collections::VecDeque;

use baztet_projectview::ProjectView;
use baztet_protos::blaze_query;
use baztet_snapshot::TargetGraph;

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

fn query_expression(specs: &[String]) -> String {
    if specs.len() == 1 {
        specs[0].clone()
    } else {
        format!("set({})", specs.join(" "))
    }
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

fn rule_srcs(rule: &blaze_query::Rule) -> Vec<String> {
    rule.attribute
        .iter()
        .filter(|a| a.name == "srcs" && !a.nodep.unwrap_or(false))
        .flat_map(|a| a.string_list_value.iter().cloned())
        .collect()
}

/// Lexicographic batching (shard_sync + target_shard_size); mirrors the plugin's batcher.
pub const DEFAULT_SHARD_SIZE: usize = 2_000;

pub fn batch_specs(specs: &[String], pv: &ProjectView) -> VecDeque<Vec<String>> {
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

/// Build the target graph from `bazel query`; returns (graph, query_failed).
/// `specs` comes pre-computed by the caller (one walk feeds the whole run).
pub async fn query_graph(
    runner: &BazelRunner,
    pv: &ProjectView,
    specs: &[String],
) -> Result<(TargetGraph, bool), BazelError> {
    let mut graph = TargetGraph::default();
    let mut batches = batch_specs(specs, pv);
    let mut query_failed = false;
    while let Some(batch) = batches.pop_front() {
        match query_batch(runner, &batch).await {
            Ok((targets, code, stderr)) => {
                if code != 0 {
                    query_failed = true;
                    eprintln!("[baztet] query exited with code {code}:\n{stderr}");
                }
                build_graph_from_targets(&mut graph, &targets, &pv.targets.excludes);
            }
            Err(e) => return Err(e),
        }
    }
    Ok((graph, query_failed))
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
#[cfg(test)]
mod tests {
    use super::*;
    use baztet_protos::blaze_query::Attribute;
    use baztet_protos::blaze_query::target::Discriminator;

    fn rule_target(name: &str, inputs: &[&str], srcs: &[&str]) -> blaze_query::Target {
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
        blaze_query::Target {
            r#type: Discriminator::Rule as i32,
            rule: Some(r),
            ..Default::default()
        }
    }

    #[test]
    fn graph_build_links_deps_and_unlinked() {
        let mut graph = TargetGraph::default();
        let rules = vec![
            rule_target("//src:a", &["//src:b", "@maven//:ext"], &["src/A.java"]),
            rule_target("//src:b", &[], &["src/B.java"]),
        ];
        build_graph_from_targets(&mut graph, &rules, &[]);
        assert_eq!(graph.len(), 2);
        let a = graph.index_of("//src:a").unwrap();
        let b = graph.index_of("//src:b").unwrap();
        assert_eq!(graph.get_by_index(a).unwrap().deps, [b]);
        assert_eq!(graph.get_by_index(a).unwrap().srcs, ["src/A.java"]);
        // external input stays unlinked (not a node)
        assert_eq!(graph.unlinked_deps(a), &["@maven//:ext".to_string()]);
    }

    #[test]
    fn graph_build_honours_excludes() {
        let mut graph = TargetGraph::default();
        build_graph_from_targets(
            &mut graph,
            &[rule_target("//gen:skip", &[], &[])],
            &["-//gen/...".to_string()],
        );
        assert!(graph.is_empty());
        assert!(graph.get_by_label("//gen:skip").is_none());
    }

    fn pv(sizes: &[(bool, Option<u32>)]) -> ProjectView {
        let (shard, size) = sizes[0];
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
}
