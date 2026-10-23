//! Project snapshot (schema v3): target graph + ide summaries + reuse metadata; bincode+zstd at the platform cache dir.

use std::collections::{BTreeMap, HashMap};

use camino::Utf8PathBuf;
use serde::{Deserialize, Serialize};
use thiserror::Error;

pub const SNAPSHOT_SCHEMA: u16 = 3;

#[derive(Debug, Error)]
pub enum SnapshotError {
    #[error("snapshot schema {found} is not supported (expected {expected})")]
    Version { found: u16, expected: u16 },
    #[error("snapshot file corrupt: {0}")]
    Corrupt(String),
    #[error("failed to write snapshot: {0}")]
    Io(#[from] std::io::Error),
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Snapshot {
    pub schema: u16,
    pub workspace_root: Utf8PathBuf,
    /// `bazel --version` output, if known
    pub bazel_version: Option<String>,
    /// target labels the project view specified (pre-expansion), for fingerprinting
    pub specified_targets: Vec<String>,
    /// phase-2 aspect data, keyed by `label!configuration`
    #[serde(default)]
    pub ide_infos: BTreeMap<String, IdeSummary>,
    /// fingerprint of the resolved project view + bazel version; unchanged
    /// fingerprint lets `sync` reuse the query graph (incremental sync)
    #[serde(default)]
    pub sync_fingerprint: Option<u64>,
    /// last-synced aspect output files per ide entry key: relpath@mtime
    #[serde(default)]
    pub ide_sources_meta: BTreeMap<String, String>,
    /// max BUILD-file mtime (unix secs) at last successful sync; detects
    /// BUILD changes without re-querying
    #[serde(default)]
    pub build_files_mtime: Option<u64>,
    /// per-package BUILD mtimes (unix secs) at last sync, when directories
    /// derive targets; delta aspect builds diff against this
    #[serde(default)]
    pub build_pkg_mtimes: Option<BTreeMap<String, u64>>,
    /// raw `bazel info` text from the last sync; exec commands reuse it to
    /// skip the info spawn (stale after a workspace/config change, the next
    /// sync refreshes it)
    #[serde(default)]
    pub bazel_info_text: Option<String>,
    /// workspace configurations seen in BEP, keyed by configuration id
    #[serde(default)]
    pub configurations: BTreeMap<String, ConfigurationLite>,
    pub graph: TargetGraph,
    /// query sync keeps going on errors: partial graph + flag
    pub has_error: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct TargetGraph {
    targets: Vec<TargetNode>,
    index: HashMap<String, u32>,
    /// rule inputs that are not themselves in the graph (external deps,
    /// generated inputs, non-target inputs)
    unlinked_deps: HashMap<u32, Vec<String>>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct TargetNode {
    /// absolute label, e.g. `//foo/bar:baz`
    pub label: String,
    /// e.g. `java_library`
    pub kind: String,
    /// BUILD file location `<path>:<line>`
    pub location: Option<String>,
    /// indices into the graph
    pub deps: Vec<u32>,
    /// source file paths relative to workspace root
    pub srcs: Vec<String>,
}

/// Trimmed projection of the aspect's `TargetIdeInfo` proto: only fields
/// the CLI serves; the full proto stays out of the snapshot.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct IdeSummary {
    /// rule kind, e.g. `java_library`
    pub kind: String,
    /// dependency labels
    pub deps: Vec<String>,
    /// source file paths relative to the workspace root
    pub srcs: Vec<String>,
    /// java jars: (interface jar, binary jar) locations or None
    pub java_jars: Vec<JarPair>,
    /// directory holding generated java sources, if any
    pub jvm_dir: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct JarPair {
    pub interface_jar: Option<String>,
    pub jar: Option<String>,
}

/// Trimmed `WorkspaceConfiguration`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ConfigurationLite {
    pub mnemonic: String,
    pub cpu: String,
}

impl TargetGraph {
    pub fn len(&self) -> usize {
        self.targets.len()
    }

    pub fn is_empty(&self) -> bool {
        self.targets.is_empty()
    }

    pub fn iter(&self) -> impl Iterator<Item = &TargetNode> {
        self.targets.iter()
    }

    pub fn get_by_label(&self, label: &str) -> Option<&TargetNode> {
        self.index.get(label).map(|&i| &self.targets[i as usize])
    }

    pub fn get_by_index(&self, idx: u32) -> Option<&TargetNode> {
        self.targets.get(idx as usize)
    }

    /// Graph index for a label, if it is a node.
    pub fn index_of(&self, label: &str) -> Option<u32> {
        self.index.get(label).copied()
    }

    /// Insert unless the label already exists; returns the index.
    pub fn get_or_insert(
        &mut self,
        label: impl Into<String>,
        kind: impl Into<String>,
        location: Option<String>,
        srcs: Vec<String>,
    ) -> u32 {
        let label = label.into();
        if let Some(&existing) = self.index.get(&label) {
            return existing;
        }
        let idx = self.targets.len() as u32;
        self.index.insert(label.clone(), idx);
        self.targets.push(TargetNode {
            label,
            kind: kind.into(),
            location,
            deps: Vec::new(),
            srcs,
        });
        idx
    }

    pub fn add_dep(&mut self, from: u32, to: u32) {
        let deps = &mut self.targets[from as usize].deps;
        if !deps.contains(&to) {
            deps.push(to);
        }
    }

    /// Record a rule input that has no node in the graph.
    pub fn add_unlinked_dep(&mut self, from: u32, label: impl Into<String>) {
        let label = label.into();
        let unlinked = self.unlinked_deps.entry(from).or_default();
        if !unlinked.contains(&label) {
            unlinked.push(label);
        }
    }

    /// Indices of targets that depend on `idx` (reverse deps).
    pub fn reverse_deps_indices(&self, idx: u32) -> Vec<u32> {
        self.targets
            .iter()
            .enumerate()
            .filter(|(_, t)| t.deps.contains(&idx))
            .map(|(i, _)| i as u32)
            .collect()
    }

    /// Labels that depend on `idx` (reverse deps).
    pub fn reverse_deps(&self, idx: u32) -> Vec<&TargetNode> {
        self.reverse_deps_indices(idx)
            .into_iter()
            .map(|i| &self.targets[i as usize])
            .collect()
    }

    /// Rule inputs of `idx` that are not themselves nodes (external deps,
    /// generated inputs, non-label inputs).
    pub fn unlinked_deps(&self, idx: u32) -> &[String] {
        self.unlinked_deps
            .get(&idx)
            .map(|v| v.as_slice())
            .unwrap_or(&[])
    }

    /// Transitive closure of deps of `idx`, BFS order.
    pub fn transitive_deps(&self, idx: u32) -> Vec<u32> {
        let mut seen = vec![idx];
        let mut queue = vec![idx];
        let mut pos = 0;
        while pos < queue.len() {
            for &d in &self.targets[queue[pos] as usize].deps {
                if !seen.contains(&d) {
                    seen.push(d);
                    queue.push(d);
                }
            }
            pos += 1;
        }
        queue
    }
}

impl Snapshot {
    pub fn new(workspace_root: Utf8PathBuf) -> Self {
        Snapshot {
            schema: SNAPSHOT_SCHEMA,
            workspace_root,
            ..Default::default()
        }
    }

    /// Serialize with compression (bincode + zstd).
    pub fn to_bytes(&self) -> Result<Vec<u8>, SnapshotError> {
        let raw = bincode::serde::encode_to_vec(self, bincode::config::standard())
            .map_err(|e| SnapshotError::Corrupt(e.to_string()))?;
        Ok(zstd::bulk::compress(&raw, 3)?)
    }

    pub fn from_bytes(bytes: &[u8]) -> Result<Snapshot, SnapshotError> {
        let raw = zstd::decode_all(bytes)?;
        let (snapshot, _): (Snapshot, usize) =
            bincode::serde::decode_from_slice(&raw, bincode::config::standard())
                .map_err(|e| SnapshotError::Corrupt(e.to_string()))?;
        if snapshot.schema != SNAPSHOT_SCHEMA {
            return Err(SnapshotError::Version {
                found: snapshot.schema,
                expected: SNAPSHOT_SCHEMA,
            });
        }
        Ok(snapshot)
    }

    /// Write under the platform cache dir (outside the repo).
    pub fn save(&self) -> Result<Utf8PathBuf, SnapshotError> {
        let path = snapshot_path(&self.workspace_root)?;
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        // write tmp then rename: a crash mid-write must not truncate the
        // snapshot into a "corrupt file" for every later command
        let tmp = path.with_extension("bin.tmp");
        std::fs::write(&tmp, self.to_bytes()?)?;
        std::fs::rename(&tmp, &path)?;
        Ok(path)
    }

    pub fn load(workspace_root: &Utf8PathBuf) -> Result<Snapshot, SnapshotError> {
        let bytes = std::fs::read(snapshot_path(workspace_root)?)?;
        Snapshot::from_bytes(&bytes)
    }

    /// Debug-friendly JSON projection.
    pub fn to_json(&self, pretty: bool) -> Result<String, SnapshotError> {
        let json = if pretty {
            serde_json::to_string_pretty(self)
        } else {
            serde_json::to_string(self)
        }
        .map_err(|e| SnapshotError::Corrupt(e.to_string()))?;
        Ok(json)
    }
}

/// Snapshot file location: `$BAZTET_CACHE_DIR/<root-hash>/snapshot.bin`, or
/// the per-OS cache dir when unset. Hash key = the canonicalized workspace
/// root path (canonicalization keeps `.`, `..`, and symlinked roots in one
/// snapshot; a non-existent root falls back to the literal path).
pub fn snapshot_path(workspace_root: &Utf8PathBuf) -> Result<Utf8PathBuf, SnapshotError> {
    use sha2::Digest;

    let base = match std::env::var("BAZTET_CACHE_DIR") {
        Ok(dir) if !dir.is_empty() => std::path::PathBuf::from(dir),
        _ => match std::env::var("XDG_CACHE_HOME") {
            Ok(dir) if !dir.is_empty() => std::path::PathBuf::from(dir).join("baztet"),
            _ => {
                let home =
                    home_dir()?.ok_or(SnapshotError::Corrupt("cannot resolve home dir".into()))?;
                cache_dir_under(home)
            }
        },
    };
    let canonical = std::fs::canonicalize(workspace_root.as_std_path())
        .unwrap_or_else(|_| workspace_root.as_std_path().to_path_buf());
    let mut hasher = sha2::Sha256::new();
    hasher.update(canonical.to_string_lossy().as_bytes());
    let hash = hex(&hasher.finalize());
    Utf8PathBuf::try_from(base.join(&hash[..16]).join("snapshot.bin"))
        .map_err(|e| SnapshotError::Corrupt(e.to_string()))
}

fn cache_dir_under(home: std::path::PathBuf) -> std::path::PathBuf {
    #[cfg(target_os = "macos")]
    {
        home.join("Library/Caches/baztet")
    }
    #[cfg(target_os = "windows")]
    {
        // %LOCALAPPDATA% when set; else keep the per-home default location
        std::env::var("LOCALAPPDATA")
            .ok()
            .filter(|d| !d.is_empty())
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|| home.join("AppData/Local"))
            .join("baztet")
    }
    #[cfg(all(not(target_os = "macos"), not(target_os = "windows")))]
    {
        home.join(".cache/baztet")
    }
}

fn home_dir() -> Result<Option<std::path::PathBuf>, SnapshotError> {
    // HOME everywhere; USERPROFILE is the Windows spelling
    Ok(std::env::var("HOME")
        .ok()
        .or_else(|| std::env::var("USERPROFILE").ok())
        .filter(|h| !h.is_empty())
        .map(std::path::PathBuf::from))
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> Snapshot {
        let mut s = Snapshot::new("/ws".into());
        s.bazel_version = Some("9.3.0".into());
        let a = s.graph.get_or_insert(
            "//a:lib",
            "java_library",
            Some("//a/BUILD:10".into()),
            vec!["a/Foo.java".into()],
        );
        let b = s
            .graph
            .get_or_insert("//b:lib", "java_library", None, vec![]);
        s.graph.add_dep(b, a);
        s.graph.add_unlinked_dep(b, "@maven//:dep");
        s.specified_targets = vec!["//a/...".into()];
        s
    }

    #[test]
    fn roundtrip() {
        let s = sample();
        let bytes = s.to_bytes().unwrap();
        let back = Snapshot::from_bytes(&bytes).unwrap();
        assert_eq!(back, s);
    }

    #[test]
    fn graph_queries() {
        let mut s = Snapshot::default();
        let a = s
            .graph
            .get_or_insert("//a:lib", "java_library", None, vec![]);
        let b = s
            .graph
            .get_or_insert("//b:lib", "java_library", None, vec![]);
        let c = s
            .graph
            .get_or_insert("//c:bin", "java_binary", None, vec![]);
        s.graph.add_dep(c, b);
        s.graph.add_dep(b, a);
        let b_node = s.graph.get_by_label("//b:lib").unwrap();
        assert_eq!(b_node.deps, [0]);
        assert_eq!(s.graph.transitive_deps(c), [c, b, a]);
        assert_eq!(s.graph.reverse_deps(a).len(), 1);
        assert_eq!(s.graph.get_by_label("//missing:lib"), None);
    }

    #[test]
    fn old_schema_errors_clean() {
        let mut s = Snapshot::new("/ws".into());
        s.schema = 2;
        let bytes = s.to_bytes().unwrap();
        match Snapshot::from_bytes(&bytes) {
            Err(SnapshotError::Version {
                found: 2,
                expected: 3,
            }) => {}
            other => panic!("expected clean Version error, got {other:?}"),
        }
    }

    #[test]
    fn corrupt_bytes_error() {
        let mut bytes = Snapshot::default().to_bytes().unwrap();
        bytes[2] ^= 0xff;
        assert!(Snapshot::from_bytes(&bytes).is_err());
    }

    #[test]
    fn get_or_insert_dedupes() {
        let mut g = TargetGraph::default();
        let a = g.get_or_insert("//a:lib", "java_library", None, vec![]);
        let a2 = g.get_or_insert("//a:lib", "java_library", None, vec![]);
        assert_eq!(a, a2);
        assert_eq!(g.len(), 1);
    }
}
