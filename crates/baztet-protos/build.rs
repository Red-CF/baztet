use std::path::PathBuf;

/// All vendored files live inside this crate so `cargo publish` packages
/// them (see THIRD_PARTY_NOTICE.md for provenance and licensing).

fn main() {
    // hermetic protoc, no system dependency
    std::env::set_var("PROTOC", protoc_bin_vendored::protoc_bin_path().expect("vendored protoc"));

    let root = PathBuf::from("protos-bazel");
    let aspect_src = PathBuf::from("protos-aspect");
    println!("cargo:rerun-if-changed={}", root.display());
    println!("cargo:rerun-if-changed={}", aspect_src.display());

    // Bazel query/BEP protos: import paths look like "src/proto/protobuf/build.proto".
    let bazel_protos = [
        root.join("src/proto/protobuf/build.proto"),
        root.join("src/proto/java/com/google/devtools/build/lib/buildeventstream/proto/build_event_stream.proto"),
        // aspect output
        aspect_src.join("sdk/proto/ide_info.proto"),
        aspect_src.join("sdk/proto/common.proto"),
    ];
    let includes = [root, aspect_src.clone()];

    let mut config = prost_build::Config::new();
    config.file_descriptor_set_path(out("descriptor.bin"));
    config.compile_protos(&bazel_protos, &includes).unwrap();

    // embed the aspect bzl sources (archive_ide.zip contents) for the deployer
    let out_dir = PathBuf::from(std::env::var("OUT_DIR").unwrap());
    copy_tree(&aspect_src, &out_dir.join("aspect"));
    let mut body = String::from("&[\n");
    for path in walk(&out_dir.join("aspect")) {
        let rel = path.strip_prefix(out_dir.join("aspect")).unwrap().to_string_lossy().replace('\\', "/");
        body.push_str(&format!(
            "    (\"{rel}\", include_bytes!(concat!(env!(\"OUT_DIR\"), \"/aspect/{rel}\"))),\n"
        ));
    }
    body.push_str("]\n");
    std::fs::write(out_dir.join("aspect_files.rs"), body).unwrap();
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
    let mut out = vec![];
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

fn out(name: &str) -> PathBuf {
    PathBuf::from(std::env::var("OUT_DIR").unwrap()).join(name)
}
