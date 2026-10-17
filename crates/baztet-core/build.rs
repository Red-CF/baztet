use std::path::PathBuf;

/// Copies the vendored intellij-aspect bzl sources (contents of the deployed
/// `archive_ide.zip`: common/, intellij/, modules/) into OUT_DIR and embeds
/// them into the binary, so the deployer is self-contained.
fn main() {
    let root = PathBuf::from("../../third_party/protos/aspect/bzl");
    println!("cargo:rerun-if-changed={}", root.display());

    let out_dir = PathBuf::from(std::env::var("OUT_DIR").unwrap());
    let mut files = String::new();
    collect(&root, &out_dir, &mut files);
    std::fs::write(out_dir.join("aspect_files.rs"), files).unwrap();
}

fn collect(root: &PathBuf, out_dir: &PathBuf, files: &mut String) {
    // copy with the same relative layout
    copy_tree(root, &out_dir.join("aspect"));

    let aspect = out_dir.join("aspect");
    let mut paths: Vec<PathBuf> = walk(&aspect);
    paths.sort();
    let mut body = String::from("&[\n");
    for path in paths {
        let rel = path
            .strip_prefix(&aspect)
            .unwrap()
            .to_string_lossy()
            .replace('\\', "/");
        body.push_str(&format!(
            "    (\"{rel}\", include_bytes!(concat!(env!(\"OUT_DIR\"), \"/aspect/{rel}\"))),\n"
        ));
    }
    body.push_str("]\n");
    files.push_str(&body);
}

fn copy_tree(from: &PathBuf, to: &PathBuf) {
    if !from.is_dir() {
        return;
    }
    std::fs::create_dir_all(to).unwrap();
    for entry in std::fs::read_dir(from).unwrap().flatten() {
        let path = entry.path();
        let dest = to.join(entry.file_name());
        if path.is_dir() {
            copy_tree(&path, &dest);
        } else {
            std::fs::copy(&path, &dest).unwrap();
        }
    }
}

fn walk(dir: &PathBuf) -> Vec<PathBuf> {
    let mut out = Vec::new();
    for entry in std::fs::read_dir(dir).unwrap().flatten() {
        let path = entry.path();
        if path.is_dir() {
            out.extend(walk(&path));
        } else {
            out.push(path);
        }
    }
    out
}