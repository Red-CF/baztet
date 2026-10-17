//! Parser and model for the `.bazelproject` project view format.
//!
//! Ported from hirschgarten's project view (`intellij.bazel.commons/.../projectview`
//! model + `intellij.bazel.projectview` PSI grammar):
//!
//! - A section header is `key:` on its own line; section entries are indented
//!   lines below it (tabs or spaces).
//! - Scalar sections may hold their value on the header line (`key: value`).
//! - List entries use `-` as the marker for an exclude.
//! - `import <path>` merges another project view (workspace-root-relative,
//!   may start with `/`); a missing file is an error. `try_import <path>`
//!   skips a missing file.
//! - `#` starts a comment.
//! - List sections may be repeated/merged; scalar sections: last wins.

use camino::Utf8PathBuf;
use std::fmt;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum ProjectViewError {
    #[error("unknown section key: {key}")]
    UnknownSection { key: String },
    #[error("cannot parse '{key}' as {expected}: {value}")]
    BadValue {
        key: String,
        expected: &'static str,
        value: String,
    },
    #[error("failed to import project view '{path}' (required import): {source}")]
    ImportFailure {
        path: String,
        source: Box<ProjectViewError>,
    },
    #[error("line {line}: {message}")]
    Parse { line: usize, message: String },
    #[error("failed to read project view file: {0}")]
    Io(#[from] std::io::Error),
}

/// Known section keys and their kind. Anything else is an error (the IDE
/// plugin accepts more for its UI features; baztet only needs these).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SectionKind {
    /// list of bazel target labels; `-` marks an exclusion
    Targets,
    /// list of directories; `-` marks an exclusion
    Directories,
    /// list of flags, plain strings
    Flags,
    /// list of plain strings (paths, etc.), no exclude semantics
    StringList,
    /// boolean singleton
    Boolean,
    /// string singleton
    String_,
    /// integer singleton
    Int,
}

fn section_kind(key: &str) -> Option<SectionKind> {
    Some(match key {
        "targets" => SectionKind::Targets,
        "directories" => SectionKind::Directories,
        "build_flags" | "test_flags" | "sync_flags" | "debug_flags" => SectionKind::Flags,
        "test_sources" | "import_run_configurations" => SectionKind::StringList,
        "derive_targets_from_directories" | "allow_manual_targets_sync" | "shard_sync" => {
            SectionKind::Boolean
        }
        "bazel_binary" | "workspace_type" | "java_language_level" => SectionKind::String_,
        "import_depth" | "target_shard_size" => SectionKind::Int,
        _ => return None,
    })
}

/// Parsed project view (own sections + resolved imports).
///
/// Scalar sections are `Option`s so that an imported value can survive when
/// the importing file does not set its own.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ProjectView {
    /// target labels to sync (include + exclude entries)
    pub targets: TargetsSection,
    /// directories for `derive_targets_from_directories`
    pub directories: DirectoriesSection,
    derive_targets_from_directories: Option<bool>,
    allow_manual_targets_sync: Option<bool>,
    shard_sync: Option<bool>,
    /// extra flags for `bazel build`
    pub build_flags: Vec<String>,
    /// extra flags for `bazel test`
    pub test_flags: Vec<String>,
    /// extra flags for `bazel build --aspects=...` during sync
    pub sync_flags: Vec<String>,
    /// extra flags for debug runs
    pub debug_flags: Vec<String>,
    /// path to the bazel binary (default: `bazel`)
    bazel_binary: Option<String>,
    /// how deep to follow imports while parsing
    import_depth: Option<u32>,
    /// targets per sharded query batch (only when `shard_sync` is enabled)
    target_shard_size: Option<u32>,
    /// additional source paths to index (tests, examples)
    pub test_sources: Vec<String>,
    /// `java` per hirschgarten; only `java` is meaningful to the CLI
    pub workspace_type: Option<String>,
    /// e.g. `JDK_21`
    pub java_language_level: Option<String>,
    /// run configuration XMLs to surface as CLI test/run recipes
    pub import_run_configurations: Vec<String>,
}

pub const DEFAULT_BAZEL_BINARY: &str = "bazel";
pub const DEFAULT_IMPORT_DEPTH: u32 = 2;

impl ProjectView {
    pub fn derive_targets_from_directories(&self) -> bool {
        self.derive_targets_from_directories.unwrap_or(false)
    }
    pub fn allow_manual_targets_sync(&self) -> bool {
        self.allow_manual_targets_sync.unwrap_or(true)
    }
    pub fn shard_sync(&self) -> bool {
        self.shard_sync.unwrap_or(false)
    }
    pub fn bazel_binary(&self) -> &str {
        self.bazel_binary.as_deref().unwrap_or(DEFAULT_BAZEL_BINARY)
    }
    pub fn import_depth(&self) -> u32 {
        self.import_depth.unwrap_or(DEFAULT_IMPORT_DEPTH)
    }
    pub fn target_shard_size(&self) -> Option<u32> {
        self.target_shard_size
    }
}

/// Merge `other` on top of `self` (import semantics: `other` is the importing
/// file's own view, `self` the imported base; own values win).
impl ProjectView {
    fn merge(&mut self, other: ProjectView) {
        self.targets.merge(other.targets);
        self.directories.merge(other.directories);
        fn take<T>(own: &mut Option<T>, other: Option<T>) {
            if other.is_some() {
                *own = other;
            }
        }
        take(&mut self.derive_targets_from_directories, other.derive_targets_from_directories);
        take(&mut self.allow_manual_targets_sync, other.allow_manual_targets_sync);
        take(&mut self.shard_sync, other.shard_sync);
        self.build_flags.extend(other.build_flags);
        self.test_flags.extend(other.test_flags);
        self.sync_flags.extend(other.sync_flags);
        self.debug_flags.extend(other.debug_flags);
        self.test_sources.extend(other.test_sources);
        self.import_run_configurations.extend(other.import_run_configurations);
        take(&mut self.workspace_type, other.workspace_type);
        take(&mut self.java_language_level, other.java_language_level);
        take(&mut self.bazel_binary, other.bazel_binary);
        take(&mut self.import_depth, other.import_depth);
        take(&mut self.target_shard_size, other.target_shard_size);
    }
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct TargetsSection {
    /// labels like `//foo:bar` or `//foo/...`
    pub includes: Vec<String>,
    /// labels excluded from the sync
    pub excludes: Vec<String>,
}

impl TargetsSection {
    fn merge(&mut self, other: TargetsSection) {
        self.includes.extend(other.includes);
        self.excludes.extend(other.excludes);
    }
}

/// A directory entry from the `directories` section.
#[derive(Debug, Clone, PartialEq)]
pub struct DirectoryEntry {
    /// directory path relative to the workspace root
    pub path: String,
    pub exclude: bool,
}

impl DirectoryEntry {
    /// Bazel target spec for this directory: `//<path>:all` (the workspace
    /// root itself, `.` or `/`, maps to `//:all`)
    pub fn as_target(&self) -> String {
        match self.path.trim_matches('/') {
            "" | "." => "//:all".to_string(),
            path => format!("//{path}:all"),
        }
    }
}

impl fmt::Display for DirectoryEntry {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.path)
    }
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct DirectoriesSection {
    pub entries: Vec<DirectoryEntry>,
}

impl DirectoriesSection {
    fn merge(&mut self, other: DirectoriesSection) {
        self.entries.extend(other.entries);
    }
}

/// Parse the project view at `path`. Import paths resolve against
/// `workspace_root` and may start with `/`.
pub fn parse(
    path: &Utf8PathBuf,
    workspace_root: &Utf8PathBuf,
) -> Result<ProjectView, ProjectViewError> {
    // the file's own `import_depth:` overrides the default (the plugin's
    // ImportDepthSection semantics); a light pre-scan finds it
    let text = std::fs::read_to_string(path)?;
    let raw = Raw::parse(&text)?;
    let depth = raw
        .scalars
        .iter()
        .find(|(k, _, _)| k == "import_depth")
        .and_then(|(_, v, _)| v.parse::<u32>().ok())
        .unwrap_or(DEFAULT_IMPORT_DEPTH);
    let _ = raw; // re-parsed by parse_with_depth_from_text below
    apply_raw_and_imports(path, workspace_root, depth, &text)
}

fn apply_raw_and_imports(
    path: &Utf8PathBuf,
    workspace_root: &Utf8PathBuf,
    remaining_depth: u32,
    _text: &str,
) -> Result<ProjectView, ProjectViewError> {
    parse_with_depth(path, workspace_root, remaining_depth)
}

/// Parse project view text directly (no file on disk). Imports whose file is
/// absent error like a required import would.
pub fn parse_text(text: &str) -> Result<ProjectView, ProjectViewError> {
    let raw = Raw::parse(text)?;
    let mut result = ProjectView::default();
    for (key, entries) in &raw.list_entries {
        apply_list(&mut result, key, entries)?;
    }
    for (key, value, line) in &raw.scalars {
        apply_scalar(&mut result, key, value, *line)?;
    }
    Ok(result)
}

fn parse_with_depth(
    path: &Utf8PathBuf,
    workspace_root: &Utf8PathBuf,
    remaining_depth: u32,
) -> Result<ProjectView, ProjectViewError> {
    let text = std::fs::read_to_string(path)?;
    let raw = Raw::parse(&text)?;

    let mut result = ProjectView::default();

    // imports are applied first: the importing file's own sections override
    if remaining_depth > 0 {
        for import in &raw.imports {
            let imported_path = resolve_import(&import.path, workspace_root);
            let imported = match imported_path {
                Some(p) => parse_with_depth(&p, workspace_root, remaining_depth - 1)?,
                None if import.optional => continue,
                None => {
                    return Err(ProjectViewError::ImportFailure {
                        path: import.path.clone(),
                        source: Box::new(ProjectViewError::Parse {
                            line: import.line,
                            message: "imported file not found".into(),
                        }),
                    });
                }
            };
            result.merge(imported);
        }
    }

    for (key, entries) in &raw.list_entries {
        apply_list(&mut result, key, entries)?;
    }
    for (key, value, line) in &raw.scalars {
        apply_scalar(&mut result, key, value, *line)?;
    }

    Ok(result)
}

struct ImportDirective {
    path: String,
    optional: bool,
    line: usize,
}

impl std::fmt::Debug for ImportDirective {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}:{} ({})", self.path, self.line, if self.optional { "try" } else { "required" })
    }
}

/// Raw intermediate before imports are merged in.
#[derive(Debug, Default)]
struct Raw {
    /// section key -> entries in file order
    list_entries: Vec<(String, Vec<String>)>,
    /// (key, value, line number)
    scalars: Vec<(String, String, usize)>,
    imports: Vec<ImportDirective>,
}

impl Raw {
    fn parse(text: &str) -> Result<Self, ProjectViewError> {
        let mut raw = Raw::default();
        let mut current_list: Option<String> = None;

        for (idx, raw_line) in text.lines().enumerate() {
            let line_no = idx + 1;
            let line = strip_comment(raw_line);
            if line.trim().is_empty() {
                continue;
            }

            let mut chars = line.chars();
            let indented = matches!(chars.next(), Some(c) if c == ' ' || c == '\t');

            let stripped = line.trim_start();
            if let Some(rest) = stripped.strip_prefix("import ") {
                raw.imports.push(ImportDirective {
                    path: strip_scalar(rest).to_string(),
                    optional: false,
                    line: line_no,
                });
                current_list = None;
                continue;
            }
            if let Some(rest) = stripped.strip_prefix("try_import ") {
                raw.imports.push(ImportDirective {
                    path: strip_scalar(rest).to_string(),
                    optional: true,
                    line: line_no,
                });
                current_list = None;
                continue;
            }

            if indented {
                let Some(key) = &current_list else {
                    return Err(ProjectViewError::Parse {
                        line: line_no,
                        message: "indented entry outside of a section".into(),
                    });
                };
                let entry = stripped.trim();
                if entry.is_empty() {
                    continue;
                }
                match raw.list_entries.iter_mut().find(|(k, _)| k == key) {
                    Some((_, entries)) => entries.push(entry.to_string()),
                    None => {
                        raw.list_entries
                            .push((key.clone(), vec![entry.to_string()]));
                    }
                }
            } else if let Some((key_part, value)) = stripped.split_once(':') {
                let key = key_part.trim();
                if key.is_empty() || key.contains(char::is_whitespace) {
                    return Err(ProjectViewError::Parse {
                        line: line_no,
                        message: format!("invalid section header: {key}"),
                    });
                }
                match section_kind(key) {
                    None => {
                        return Err(ProjectViewError::UnknownSection { key: key.to_string() });
                    }
                    Some(SectionKind::Targets)
                    | Some(SectionKind::Directories)
                    | Some(SectionKind::Flags)
                    | Some(SectionKind::StringList) => {
                        current_list = Some(key.to_string());
                        // allow the first entry inline: `targets: //foo`
                        let inline = strip_scalar(value);
                        if !inline.is_empty() {
                            raw.list_entries.push((key.to_string(), vec![inline.to_string()]));
                        }
                    }
                    Some(_) => {
                        current_list = None;
                        raw.scalars
                            .push((key.to_string(), strip_scalar(value).to_string(), line_no));
                    }
                }
            } else {
                return Err(ProjectViewError::Parse {
                    line: line_no,
                    message: format!("expected a section header, got: {stripped}"),
                });
            }
        }

        Ok(raw)
    }
}

/// Remove a trailing `# comment`.
fn strip_comment(line: &str) -> &str {
    // a target label or flag value never contains a bare `#`
    match line.find('#') {
        Some(pos) => &line[..pos],
        None => line,
    }
}

fn apply_list(result: &mut ProjectView, key: &str, entries: &[String]) -> Result<(), ProjectViewError> {
    match key {
        "targets" => {
            for entry in entries {
                let (exclude, label) = split_exclude(entry);
                if exclude {
                    result.targets.excludes.push(label.to_string());
                } else {
                    result.targets.includes.push(label.to_string());
                }
            }
        }
        "directories" => {
            for entry in entries {
                let (exclude, path) = split_exclude(entry);
                result.directories.entries.push(DirectoryEntry {
                    path: path.to_string(),
                    exclude,
                });
            }
        }
        "build_flags" => result.build_flags.extend(entries.iter().cloned()),
        "test_flags" => result.test_flags.extend(entries.iter().cloned()),
        "sync_flags" => result.sync_flags.extend(entries.iter().cloned()),
        "debug_flags" => result.debug_flags.extend(entries.iter().cloned()),
        "test_sources" => result.test_sources.extend(entries.iter().cloned()),
        "import_run_configurations" => {
            result.import_run_configurations.extend(entries.iter().cloned())
        }
        _ => unreachable!("validated earlier"),
    }
    Ok(())
}

fn apply_scalar(
    result: &mut ProjectView,
    key: &str,
    value: &str,
    line: usize,
) -> Result<(), ProjectViewError> {
    match key {
        "derive_targets_from_directories" => {
            result.derive_targets_from_directories = Some(parse_bool(key, value, line)?)
        }
        "allow_manual_targets_sync" => {
            result.allow_manual_targets_sync = Some(parse_bool(key, value, line)?)
        }
        "shard_sync" => result.shard_sync = Some(parse_bool(key, value, line)?),
        "bazel_binary" => result.bazel_binary = Some(value.to_string()),
        "import_depth" => {
            result.import_depth = Some(
                value
                    .parse()
                    .map_err(|_| ProjectViewError::BadValue { key: key.to_string(), expected: "an integer", value: value.to_string() })?,
            )
        }
        "target_shard_size" => {
            result.target_shard_size = Some(
                value
                    .parse()
                    .map_err(|_| ProjectViewError::BadValue { key: key.to_string(), expected: "an integer", value: value.to_string() })?,
            )
        }
        // carried, no CLI behavior yet
        "workspace_type" => result.workspace_type = Some(value.to_string()),
        "java_language_level" => result.java_language_level = Some(value.to_string()),
        _ => unreachable!("validated earlier"),
    }
    Ok(())
}

fn parse_bool(key: &str, value: &str, line: usize) -> Result<bool, ProjectViewError> {
    match value {
        "true" => Ok(true),
        "false" => Ok(false),
        other => Err(ProjectViewError::Parse {
            line,
            message: format!("cannot parse '{key}' as true/false, got: {other}"),
        }),
    }
}

/// Split a leading `-` exclude prefix from a list entry.
fn split_exclude(entry: &str) -> (bool, &str) {
    match entry.strip_prefix('-') {
        Some(rest) => (true, rest),
        None => (false, entry),
    }
}

fn strip_scalar(value: &str) -> &str {
    value.trim()
}

fn resolve_import(path: &str, workspace_root: &Utf8PathBuf) -> Option<Utf8PathBuf> {
    let trimmed = path.strip_prefix('/').unwrap_or(path);
    let candidate = workspace_root.join(trimmed);
    candidate.is_file().then_some(candidate)
}

#[cfg(test)]
mod tests {
    use super::*;

    const FULL: &str = "\
import /other.bazelproject
try_import /missing.bazelproject

targets:
    //x:lib
    -//x:excluded

build_flags:
    --flag1=1
    --flag2

derive_targets_from_directories: true

directories:
    \ta/b
    -excluded/dir
";

    fn apply_all(mut pv: ProjectView, raw: &Raw) -> ProjectView {
        for (key, entries) in &raw.list_entries {
            apply_list(&mut pv, key, entries).unwrap();
        }
        pv
    }

    #[test]
    fn raw_parse_sections() {
        let raw = Raw::parse(FULL).unwrap();
        assert_eq!(raw.imports.len(), 2);
        assert_eq!(raw.imports[0].optional, false);
        assert_eq!(raw.imports[1].optional, true);
        let mut pv = ProjectView::default();
        for (key, entries) in &raw.list_entries {
            apply_list(&mut pv, key, entries).unwrap();
        }
        for (key, value, line) in &raw.scalars {
            apply_scalar(&mut pv, key, value, *line).unwrap();
        }
        assert_eq!(pv.targets.includes, ["//x:lib"]);
        assert_eq!(pv.targets.excludes, ["//x:excluded"]);
        assert_eq!(pv.build_flags, ["--flag1=1", "--flag2"]);
        assert!(pv.derive_targets_from_directories());
        let dirs = &pv.directories.entries;
        assert_eq!(dirs[0].as_target(), "//a/b:all");
        assert_eq!(dirs[0].exclude, false);
        assert_eq!(dirs[1].exclude, true);
    }

    #[test]
    fn comments_and_inline_entries() {
        let raw = Raw::parse("targets: //inline:target\n# comment\nbuild_flags:\n    --a # trailing\n").unwrap();
        assert_eq!(raw.list_entries.len(), 2);
        assert_eq!(raw.list_entries[0].1, ["//inline:target"]);
        assert_eq!(raw.list_entries[1].1, ["--a"]);
    }

    #[test]
    fn unknown_key_errors() {
        let err = Raw::parse("targets:\n    //a:b\nbogus_section:\n    x\n");
        assert!(matches!(err, Err(ProjectViewError::UnknownSection { .. })));
    }

    #[test]
    fn merge_own_wins() {
        // base sets a scalar; the own view overrides it via merge
        let mut base = ProjectView {
            import_depth: Some(5),
            build_flags: vec!["--base".into()],
            ..Default::default()
        };
        let own = ProjectView {
            import_depth: Some(1),
            build_flags: vec!["--own".into()],
            ..Default::default()
        };
        base.merge(own);
        assert_eq!(base.import_depth(), 1);
        assert_eq!(base.build_flags, ["--base", "--own"]);
    }

    #[test]
    fn import_chain_files() {
        use std::fs;
        let tmp = std::env::temp_dir().join("baztet-pv-import-test");
        let ws = camino::Utf8PathBuf::try_from(tmp.clone()).unwrap().join("ws");
        let _ = fs::remove_dir_all(&ws);
        fs::create_dir_all(&ws).unwrap();
        fs::write(ws.join("base.bazelproject"), "import_depth: 5\nbuild_flags:\n    --base\n")
            .unwrap();
        fs::write(ws.join("own.bazelproject"), "import /base.bazelproject\nbuild_flags:\n    --own\n")
            .unwrap();

        let view = parse(&ws.join("own.bazelproject"), &ws).unwrap();
        assert_eq!(view.import_depth(), 5); // imported value survives
        assert_eq!(view.build_flags, ["--base", "--own"]); // own flags land after

        // missing required import errors; missing try_import is skipped
        fs::write(ws.join("bad.bazelproject"), "import /missing.bazelproject\ntargets:\n    //a:b\n").unwrap();
        assert!(matches!(
            parse(&ws.join("bad.bazelproject"), &ws),
            Err(ProjectViewError::ImportFailure { .. })
        ));
        fs::write(ws.join("ok.bazelproject"), "try_import /missing.bazelproject\ntargets:\n    //a:b\n").unwrap();
        assert!(parse(&ws.join("ok.bazelproject"), &ws).is_ok());
        let _ = fs::remove_dir_all(&ws);
    }

    #[test]
    fn split_exclude_labels() {
        assert_eq!(split_exclude("-//x"), (true, "//x"));
        assert_eq!(split_exclude("//x"), (false, "//x"));
    }
}