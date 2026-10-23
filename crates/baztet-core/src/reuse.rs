//! Incremental reuse ladder and sync fingerprint: pure functions over
//! recorded-vs-current facts, no I/O and no runner — table-testable.

use std::collections::BTreeMap;

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

/// Facts recorded in the last sync's snapshot.
#[derive(Debug, Clone, Default)]
pub struct RecordedFacts {
    /// max BUILD-file mtime (unix secs) of the last walk
    pub build_mtime: Option<u64>,
    /// per-package BUILD mtimes, when directories derive targets
    /// (None on old snapshots and derive-off syncs)
    pub pkg_mtimes: Option<BTreeMap<String, u64>>,
}

/// Facts of the current workspace state.
#[derive(Debug, Clone, Default)]
pub struct CurrentFacts {
    /// max BUILD-file mtime of this walk (None = derive off)
    pub build_mtime: Option<u64>,
    /// per-package BUILD mtimes of this walk (empty = derive off)
    pub pkg_mtimes: BTreeMap<String, u64>,
    /// whether the project view has `directories:` entries
    pub directories_configured: bool,
}

/// Pure reuse-ladder decision over recorded snapshot facts vs the current run.
///
/// - fingerprint mismatch (or empty graph) -> FullQuery, always
/// - directories configured: exact per-package comparison when a recorded
///   map exists (catches added/removed packages with preserved mtimes);
///   max-mtime comparison on old snapshots without a map
/// - no directories: nothing can have drifted via the walker
/// - BUILD state unchanged AND we don't need (or have) aspect data ->
///   Everything (touch nothing)
/// - BUILD state unchanged but aspect refresh required -> Graph
pub fn decide_reuse(
    fingerprints_match: bool,
    recorded: &RecordedFacts,
    current: &CurrentFacts,
    run_aspect_phase: bool,
    has_ide_infos: bool,
) -> Reuse {
    if !fingerprints_match {
        return Reuse::FullQuery;
    }
    let build_fresh = if !current.directories_configured {
        // derive off: nothing can have drifted via the walker
        true
    } else if !current.pkg_mtimes.is_empty() && current.build_mtime.is_some() {
        match &recorded.pkg_mtimes {
            // exact set comparison: changed, added or removed BUILD files
            Some(rec) => rec == &current.pkg_mtimes,
            None => match (recorded.build_mtime, current.build_mtime) {
                (Some(r), Some(c)) => c <= r,
                _ => false,
            },
        }
    } else {
        // directories configured but the walk found nothing: BUILD files
        // gone, the graph is stale
        false
    };
    if !build_fresh {
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
/// from an older toolchain just misses the ladder (one resync, no wrong reuse).
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

    fn facts(rec: Option<u64>, pkgs: BTreeMap<String, u64>) -> RecordedFacts {
        RecordedFacts {
            build_mtime: rec,
            pkg_mtimes: (!pkgs.is_empty()).then_some(pkgs),
        }
    }

    fn cur(max: Option<u64>, pkgs: BTreeMap<String, u64>, dirs: bool) -> CurrentFacts {
        CurrentFacts {
            build_mtime: max,
            pkg_mtimes: pkgs,
            directories_configured: dirs,
        }
    }

    fn map_of(pairs: &[(&str, u64)]) -> BTreeMap<String, u64> {
        pairs.iter().map(|(p, m)| ((*p).into(), *m)).collect()
    }

    #[test]
    fn mismatched_fingerprint_always_full_query() {
        let recorded = facts(Some(1), map_of(&[("a", 1)]));
        assert_eq!(
            decide_reuse(false, &recorded, &cur(Some(1), map_of(&[("a", 1)]), true), true, true),
            Reuse::FullQuery
        );
    }

    #[test]
    fn unchanged_pkg_map_reuse_everything() {
        let recorded = facts(Some(1), map_of(&[("a", 1), ("b", 2)]));
        assert_eq!(
            decide_reuse(true, &recorded, &cur(Some(1), map_of(&[("a", 1), ("b", 2)]), true), true, true),
            Reuse::Everything
        );
    }

    #[test]
    fn added_package_with_preserved_mtime_is_stale() {
        // the recorded max is 3 and the new package's BUILD is older: only
        // the per-package map catches the addition
        let recorded = facts(Some(3), map_of(&[("a", 3)]));
        assert_eq!(
            decide_reuse(true, &recorded, &cur(Some(3), map_of(&[("a", 3), ("b", 1)]), true), true, true),
            Reuse::FullQuery
        );
    }

    #[test]
    fn changed_package_is_stale() {
        let recorded = facts(Some(5), map_of(&[("a", 1)]));
        assert_eq!(
            decide_reuse(true, &recorded, &cur(Some(5), map_of(&[("a", 5)]), true), true, true),
            Reuse::FullQuery
        );
    }

    #[test]
    fn removed_package_is_stale() {
        let recorded = facts(Some(3), map_of(&[("a", 3), ("b", 3)]));
        assert_eq!(
            decide_reuse(true, &recorded, &cur(Some(3), map_of(&[("a", 3)]), true), true, true),
            Reuse::FullQuery
        );
    }

    #[test]
    fn all_builds_deleted_is_stale() {
        let recorded = facts(Some(3), map_of(&[("a", 3)]));
        assert_eq!(
            decide_reuse(true, &recorded, &cur(None, map_of(&[]), true), true, true),
            Reuse::FullQuery
        );
    }

    #[test]
    fn max_mtime_fallback_for_mapless_snapshots() {
        // old snapshots carry only the max mtime
        let recorded = facts(Some(9), map_of(&[]));
        assert_eq!(
            decide_reuse(true, &recorded, &cur(Some(9), map_of(&[("a", 1)]), true), true, true),
            Reuse::Everything
        );
        assert_eq!(
            decide_reuse(true, &recorded, &cur(Some(10), map_of(&[("a", 1)]), true), true, true),
            Reuse::FullQuery
        );
    }

    #[test]
    fn no_directories_no_walker() {
        let recorded = facts(Some(9), map_of(&[]));
        assert_eq!(
            decide_reuse(true, &recorded, &cur(None, map_of(&[]), false), true, true),
            Reuse::Everything
        );
        // derive-off has no recorded facts either; nothing can drift
        assert_eq!(
            decide_reuse(true, &facts(None, map_of(&[])), &cur(None, map_of(&[]), false), true, true),
            Reuse::Everything
        );
    }

    #[test]
    fn aspect_refresh_when_no_data() {
        let recorded = facts(Some(9), map_of(&[("a", 9)]));
        assert_eq!(
            decide_reuse(true, &recorded, &cur(Some(9), map_of(&[("a", 9)]), true), true, false),
            Reuse::Graph
        );
        // --fast with no ide data: nothing aspect-y to refresh, reuse all
        assert_eq!(
            decide_reuse(true, &recorded, &cur(Some(9), map_of(&[("a", 9)]), true), false, false),
            Reuse::Everything
        );
    }
}