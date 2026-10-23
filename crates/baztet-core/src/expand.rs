//! Expands project-view directories into concrete `//pkg:all` specs (walks packages honoring excludes), mirroring the plugin's PackageLister.

use std::collections::{BTreeMap, HashSet};
use std::path::Path;

use baztet_projectview::ProjectView;

const SKIP_DIRS: [&str; 6] = [
    ".git",
    ".bazel",
    ".bazelbin",
    ".bazel-out",
    "bazel-bin",
    "bazel-out",
];

/// Expand `directories:` into specs: shared walk, two public consumers.
fn walk_specs(pv: &ProjectView, workspace_root: &str) -> (Vec<String>, BTreeMap<String, u64>) {
    if !pv.derive_targets_from_directories() {
        return (vec![], BTreeMap::new());
    }

    let excludes: HashSet<&str> = pv
        .directories
        .entries
        .iter()
        .filter(|e| e.exclude)
        .map(|e| e.path.trim_matches('/'))
        .collect();

    let mut specs: Vec<String> = Vec::new();
    let mut mtimes = BTreeMap::new();
    for entry in &pv.directories.entries {
        if entry.exclude {
            continue;
        }
        let rel = entry.path.trim();
        let rel = rel.trim_matches('/');
        let dir = Path::new(workspace_root).join(rel);
        walk_packages(&dir, rel, &excludes, &mut specs, &mut mtimes);
    }
    specs.sort();
    specs.dedup();
    (specs, mtimes)
}

/// All `//pkg:all` specs for the included directories, minus excluded
/// directory subtrees (sorted, deduped) plus the max BUILD mtime (unix
/// secs) seen by the walk (incremental sync freshness check).
pub fn expand_directory_targets(
    pv: &ProjectView,
    workspace_root: &str,
) -> (Vec<String>, Option<u64>) {
    let (specs, mtimes) = walk_specs(pv, workspace_root);
    (specs, mtimes.values().max().copied())
}

/// Per-package BUILD-file mtimes (unix secs); empty when derive is off.
/// Delta aspect builds compare this against the snapshot's recorded map.
pub fn package_build_mtimes(pv: &ProjectView, workspace_root: &str) -> BTreeMap<String, u64> {
    walk_specs(pv, workspace_root).1
}

/// DFS: emit `//<pkg>:all` for every directory holding a BUILD file.
fn walk_packages(
    dir: &Path,
    pkg: &str,
    excludes: &HashSet<&str>,
    out: &mut Vec<String>,
    mtimes: &mut BTreeMap<String, u64>,
) {
    if is_package_dir(dir) {
        out.push(pkg_to_spec(pkg));
        if let Some(m) = build_file_mtime(dir) {
            mtimes.insert(pkg.to_string(), m);
        }
    }
    let Ok(read) = std::fs::read_dir(dir) else {
        return;
    };
    let mut children: Vec<_> = read.flatten().collect();
    children.sort_by_key(|c| c.file_name());
    for child in children {
        let Ok(file_type) = child.file_type() else {
            continue;
        };
        if !file_type.is_dir() {
            continue;
        }
        let name = child.file_name();
        let name = match name.to_str() {
            Some(n) => n,
            None => continue,
        };
        if SKIP_DIRS.contains(&name) || name.starts_with("bazel-") || name.starts_with('.') {
            continue;
        }
        let child_pkg = if pkg.is_empty() {
            name.to_string()
        } else {
            format!("{pkg}/{name}")
        };
        if is_excluded(&child_pkg, excludes) {
            continue;
        }
        walk_packages(&child.path(), &child_pkg, excludes, out, mtimes);
    }
}

/// `BUILD`/`BUILD.bazel` mtime in unix seconds (0 when unknown).
fn build_file_mtime(dir: &Path) -> Option<u64> {
    let path = if dir.join("BUILD").is_file() {
        dir.join("BUILD")
    } else {
        dir.join("BUILD.bazel")
    };
    std::fs::metadata(path)
        .ok()
        .and_then(|m| m.modified().ok())
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs())
}

fn is_excluded(pkg: &str, excludes: &HashSet<&str>) -> bool {
    excludes
        .iter()
        .any(|ex| pkg == *ex || pkg.starts_with(&format!("{ex}/")))
}

fn is_package_dir(dir: &Path) -> bool {
    dir.join("BUILD").is_file() || dir.join("BUILD.bazel").is_file()
}

fn pkg_to_spec(pkg: &str) -> String {
    match pkg.trim_matches('.') {
        "" => "//:all".to_string(),
        pkg => format!("//{pkg}:all"),
    }
}

/// Filter synced target labels against the project view's exclude patterns
/// (`targets:` entries starting with `-`, incl. `//path/...` wildcards).
pub fn label_is_excluded(label: &str, excludes: &[String]) -> bool {
    fn label_in_package(label: &str, pkg: &str) -> bool {
        // `//pkg` forms: the package's own label, a subpackage, or a rule in it
        label == pkg
            || label.starts_with(&format!("{pkg}/"))
            || label.starts_with(&format!("{pkg}:"))
    }

    excludes.iter().any(|ex| {
        let ex = ex.strip_prefix('-').unwrap_or(ex);
        if let Some(prefix) = ex.strip_suffix("/...") {
            label_in_package(label, prefix)
        } else if let Some(pkg) = ex.strip_suffix(":all") {
            let pkg = pkg.trim_start_matches("//");
            // the //pkg:all wildcard covers subpackages AND rules in the
            // package itself (//pkg:lib)
            label_in_package(label, &format!("//{pkg}"))
        } else {
            label == ex
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sync::target_specs;
    use baztet_projectview::parse_text;

    fn hand_pv(dirs: &[(&str, bool)], derive: bool, excludes: &[&str]) -> ProjectView {
        let text = format!(
            "{}\n\ndirectories:\n{}\ntargets:\n{}",
            if derive {
                "derive_targets_from_directories: true"
            } else {
                ""
            },
            dirs.iter()
                .map(|(p, ex)| if *ex {
                    format!("    -{p}")
                } else {
                    format!("    {p}")
                })
                .collect::<Vec<_>>()
                .join("\n"),
            if excludes.is_empty() {
                String::new()
            } else {
                excludes
                    .iter()
                    .map(|e| format!("    {e}"))
                    .collect::<Vec<_>>()
                    .join("\n")
            },
        );
        parse_text(&text).unwrap()
    }

    fn tree(base: &std::path::Path, dirs: &[&str]) {
        for d in dirs {
            let p = base.join(d);
            std::fs::create_dir_all(&p).unwrap();
            std::fs::write(p.join("BUILD.bazel"), "").unwrap();
        }
        std::fs::write(base.join("BUILD.bazel"), "").unwrap();
    }

    #[test]
    fn expanding_and_target_specs() {
        let tmp = std::env::temp_dir().join("baztet-expand-test");
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).unwrap();
        tree(
            &tmp,
            &["services/a", "services/a/sub", "services/b", "libs/x"],
        );

        let pv = hand_pv(&[("services/a", false), ("services/b", false)], true, &[]);
        let specs = expand_directory_targets(&pv, tmp.to_str().unwrap()).0;
        assert!(specs.contains(&"//services/a:all".to_string()));
        assert!(specs.contains(&"//services/a/sub:all".to_string()));
        assert!(specs.contains(&"//services/b:all".to_string()));
        assert!(!specs.iter().any(|s| s.contains("libs")));

        // exclude services/a subtree by only listing services/b
        let pv = hand_pv(&[("services/a", true), ("services/b", false)], true, &[]);
        let specs = expand_directory_targets(&pv, tmp.to_str().unwrap()).0;
        assert!(specs.contains(&"//services/b:all".to_string()));
        assert!(!specs.iter().any(|s| s.contains("services/a")));

        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn target_specs_combines() {
        // real tree needed: directory expansion walks the filesystem
        let tmp = std::env::temp_dir().join("baztet-spec-expand-test");
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).unwrap();
        tree(&tmp, &["pkg"]);
        let pv = hand_pv(&[("pkg", false)], true, &["-//pkg/gen"]);
        let specs = target_specs(&pv, tmp.to_str().unwrap());
        // excludes filter post-query, not the spec list itself
        assert_eq!(specs, ["//pkg:all"]);
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn wildcard_covers_package_root_rules() {
        // bug regression: //-pkg:all wildcards must exclude rules in the
        // package itself, not only in subpackages
        assert!(label_is_excluded("//pkg:lib", &["-//pkg:all".into()]));
        assert!(label_is_excluded("//pkg:lib", &["-//pkg/...".into()]));
        assert!(label_is_excluded("//pkg", &["-//pkg/...".into()]));
    }

    #[test]
    fn excludes_match() {
        assert!(label_is_excluded(
            "//pkg/gen/a:lib",
            &["-//pkg/gen/...".into()]
        ));
        assert!(!label_is_excluded(
            "//pkg/other:lib",
            &["-//pkg/gen/...".into()]
        ));
        assert!(label_is_excluded(
            "//pkg/gen/x:a",
            &["-//pkg/gen:all".into()]
        ));
        assert!(label_is_excluded("//tool:x", &["-//tool:x".into()]));
        assert!(!label_is_excluded("//tool:x", &["-//tool:y".into()]));
    }
}
