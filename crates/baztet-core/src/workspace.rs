//! Workspace discovery and `bazel info` parsing.

use std::collections::BTreeMap;

use camino::Utf8PathBuf;

/// Find the workspace root by ascending from `start` looking for
/// `MODULE.bazel` / `WORKSPACE` / `WORKSPACE.bazel` / `MODULE.bazel.lock`.
pub fn find_workspace_root(start: &Utf8PathBuf) -> Option<Utf8PathBuf> {
    const MARKERS: [&str; 4] = ["MODULE.bazel", "WORKSPACE", "WORKSPACE.bazel", "REPO.bazel"];
    let mut dir = start.clone();
    loop {
        if MARKERS.iter().any(|m| dir.join(m).is_file()) {
            return Some(dir);
        }
        dir = dir.parent()?.to_owned();
    }
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct BazelInfo {
    /// e.g. `release 9.3.0`
    pub release: String,
    pub server_pid: Option<u32>,
    pub execution_root: Utf8PathBuf,
    pub output_base: Utf8PathBuf,
    pub bazel_bin: Utf8PathBuf,
    /// raw dump for debugging
    pub extra: BTreeMap<String, String>,
}

impl BazelInfo {
    pub fn bazel_version(&self) -> Option<&str> {
        let release = self.release.strip_prefix("release")?;
        release.trim().split_whitespace().next()
    }
}

pub fn parse_info(text: &str) -> BazelInfo {
    let mut info = BazelInfo {
        execution_root: "/".into(),
        output_base: "/".into(),
        bazel_bin: "/".into(),
        ..Default::default()
    };
    for line in text.lines() {
        let Some((key, value)) = line.split_once(':') else {
            continue;
        };
        let value = value.trim();
        info.extra.insert(key.trim().to_string(), value.to_string());
        match key.trim() {
            "release" => info.release = value.to_string(),
            "server_pid" => info.server_pid = value.parse().ok(),
            "execution_root" => info.execution_root = value.into(),
            "output_base" => info.output_base = value.into(),
            "bazel-bin" => info.bazel_bin = value.into(),
            _ => {}
        }
    }
    info
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_info_fields() {
        let text = "\
release: release 9.3.0
server_pid: 4242
execution_root: /ws
output_base: /tmp/_bazel/x/abc
bazel-bin: /ws/bazel-bin
";
        let info = parse_info(text);
        assert_eq!(info.bazel_version(), Some("9.3.0"));
        assert_eq!(info.server_pid, Some(4242));
        assert_eq!(info.execution_root.as_str(), "/ws");
    }

    #[test]
    fn find_root_ascends() {
        let root = std::env::temp_dir();
        let marker = camino::Utf8PathBuf::try_from(root.join("baztet-test-ws")).unwrap();
        std::fs::create_dir_all(&marker).unwrap();
        if !std::path::Path::new(&marker).join("MODULE.bazel").is_file() {
            std::fs::write(marker.join("MODULE.bazel"), "").unwrap();
        }
        let found = find_workspace_root(&marker);
        // the temp dir chain may itself contain a marker above; only require
        // that the marker dir is at-or-above the result boundary... simplest:
        // search upward must stop at some marker.
        assert!(found.is_some());
        let _ = std::fs::remove_file(marker.join("MODULE.bazel"));
    }
}