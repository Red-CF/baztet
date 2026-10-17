//! baztet project snapshot: target graph + persistence.
//!
//! Version 2 = query sync data + phase-2 aspect (TargetIdeInfo summaries).

use std::collections::{BTreeMap, HashMap};

use camino::Utf8PathBuf;
use serde::{Deserialize, Serialize};
use thiserror::Error;

pub const SNAPSHOT_SCHEMA: u16 = 2;
/// on-disk location, relative to the workspace root
pub const SNAPSHOT_PATH: &str = ".baztet/snapshot.bin";

#[derive(Debug, Error)]
pub enum SnapshotError {
    #[error("snapshot schema {found} is not supported (expected {expected})")]
    Version { found: u16, expected: u16 },
    #[error("snapshot file corrupt: {0}")]
    Corrupt(String),
    #[error("failed to write snapshot: {0}")]
    Io(#[from] std::io::Error),
}

/// A synced project: targets + edges.
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
    /// workspace configurations seen in BEP, keyed by configuration id
    #[serde(default)]
    pub configurations: BTreeMap<String, ConfigurationLite>,
    pub graph: TargetGraph,
    /// query sync keeps going on errors: partial graph + flag
    pub has_error: bool,
}

/// Compact target graph: labels interned into `Vec<TargetNode>`.
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

    /// Labels that depend on `idx` (reverse deps).
    pub fn reverse_deps(&self, idx: u32) -> Vec<&TargetNode> {
        self.targets
            .iter()
            .filter(|t| t.deps.contains(&idx))
            .collect()
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

    /// Write to `<workspace_root>/<SNAPSHOT_PATH>`, creating dirs.
    pub fn save(&self) -> Result<Utf8PathBuf, SnapshotError> {
        let path = self.workspace_root.join(SNAPSHOT_PATH);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&path, self.to_bytes()?)?;
        Ok(path)
    }

    pub fn load(workspace_root: &Utf8PathBuf) -> Result<Snapshot, SnapshotError> {
        let bytes = std::fs::read(workspace_root.join(SNAPSHOT_PATH))?;
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

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> Snapshot {
        let mut s = Snapshot::new("/ws".into());
        s.bazel_version = Some("9.3.0".into());
        let a = s.graph.get_or_insert("//a:lib", "java_library", Some("//a/BUILD:10".into()), vec!["a/Foo.java".into()]);
        let b = s.graph.get_or_insert("//b:lib", "java_library", None, vec![]);
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
        let a = s.graph.get_or_insert("//a:lib", "java_library", None, vec![]);
        let b = s.graph.get_or_insert("//b:lib", "java_library", None, vec![]);
        let c = s.graph.get_or_insert("//c:bin", "java_binary", None, vec![]);
        s.graph.add_dep(c, b);
        s.graph.add_dep(b, a);
        let b_node = s.graph.get_by_label("//b:lib").unwrap();
        assert_eq!(b_node.deps, [0]);
        assert_eq!(s.graph.transitive_deps(c), [c, b, a]);
        assert_eq!(s.graph.reverse_deps(a).len(), 1);
        assert_eq!(s.graph.get_by_label("//missing:lib"), None);
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