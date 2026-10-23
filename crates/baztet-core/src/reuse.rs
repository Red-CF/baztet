//! Incremental reuse ladder and sync fingerprint: pure functions over
//! recorded-vs-current facts, no I/O and no runner — table-testable.

use baztet_projectview::ProjectView;
use baztet_snapshot::Snapshot;
use crate::bazel::BazelRunner;

/// What one `sync` run produced: the snapshot, reuse level, timings, parse counts.
#[derive(Debug, Clone)]
pub struct SyncReport {
    pub snapshot: Snapshot,
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

/// Pure reuse-ladder decision over recorded snapshot facts vs the current run.
///
/// - fingerprint mismatch (or empty graph) -> FullQuery, always
/// - recorded BUILD mtimes missing while directories are configured -> stale
///   walker state, cannot prove freshness -> FullQuery
/// - BUILD mtimes unchanged AND we don't need (or have) aspect data ->
///   Everything (touch nothing)
/// - BUILD mtimes unchanged but aspect refresh required -> Graph
pub fn decide_reuse(
    fingerprints_match: bool,
    recorded_mtime: Option<u64>,
    current_mtime: Option<u64>,
    directories_configured: bool,
    run_aspect_phase: bool,
    has_ide_infos: bool,
) -> Reuse {
    if !fingerprints_match {
        return Reuse::FullQuery;
    }
    let builds_unchanged = match (recorded_mtime, current_mtime) {
        (Some(recorded), Some(current)) => current <= recorded,
        // walker state missing: reuse only when recorded mtimes exist or no
        // directories are configured (nothing can have drifted)
        (r, None) => r.is_some() || !directories_configured,
        (None, Some(_)) => false,
    };
    if !builds_unchanged {
        return Reuse::FullQuery;
    }
    if !run_aspect_phase || has_ide_infos {
        Reuse::Everything
    } else {
        Reuse::Graph
    }
}

/// Stable fingerprint of everything that changes the query result.
///
/// `DefaultHasher::new()` is deterministic per rustc build; a fingerprint
/// from an older toolchain simply misses the reuse ladder (one full
/// resync), never produces a wrong reuse.
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mismatched_fingerprint_always_full_query() {
        assert_eq!(
            decide_reuse(false, Some(1), Some(1), true, true, true),
            Reuse::FullQuery
        );
        assert_eq!(
            decide_reuse(false, Some(9), Some(1), false, false, true),
            Reuse::FullQuery
        );
    }

    #[test]
    fn unchanged_builds_with_ide_infos_reuse_everything() {
        // mtime equal or regressed counts as unchanged
        assert_eq!(
            decide_reuse(true, Some(10), Some(10), true, true, true),
            Reuse::Everything
        );
        assert_eq!(
            decide_reuse(true, Some(10), Some(5), true, true, true),
            Reuse::Everything
        );
    }

    #[test]
    fn unchanged_builds_without_aspect_data_refreshes_aspect() {
        assert_eq!(
            decide_reuse(true, Some(10), Some(10), true, true, false),
            Reuse::Graph
        );
        // --fast with no ide data: nothing aspect-y to refresh, reuse all
        assert_eq!(
            decide_reuse(true, Some(10), Some(10), true, false, false),
            Reuse::Everything
        );
    }

    #[test]
    fn newer_build_mtime_is_stale() {
        assert_eq!(
            decide_reuse(true, Some(5), Some(6), true, true, true),
            Reuse::FullQuery
        );
    }

    #[test]
    fn missing_recorded_mtime_with_directories_is_stale() {
        assert_eq!(
            decide_reuse(true, None, Some(6), true, true, true),
            Reuse::FullQuery
        );
    }

    #[test]
    fn no_directories_no_walker_means_nothing_can_drift() {
        assert_eq!(decide_reuse(true, Some(10), None, false, true, true), Reuse::Everything);
        assert_eq!(decide_reuse(true, None, None, false, true, true), Reuse::Everything);
        // recorded mtime without directories configured but directories empty
        // (r.is_some()) also reuses
        assert_eq!(decide_reuse(true, Some(10), None, true, true, true), Reuse::Everything);
    }
}