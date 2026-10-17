use std::path::PathBuf;

fn proto_root() -> PathBuf {
    PathBuf::from("../../third_party/protos")
}

fn main() {
    // hermetic protoc, no system dependency
    std::env::set_var(
        "PROTOC",
        protoc_bin_vendored::protoc_bin_path().expect("vendored protoc"),
    );

    let root = proto_root();
    let bazel = root.join("bazel");
    let aspect = root.join("aspect");

    // Bazel query protos: import paths look like "src/proto/protobuf/build.proto".
    let bazel_protos = [
        // phase-1 query sync
        bazel.join("src/proto/protobuf/build.proto"),
        // BEP
        bazel.join("src/proto/java/com/google/devtools/build/lib/buildeventstream/proto/build_event_stream.proto"),
        // aspect output
        aspect.join("sdk/proto/ide_info.proto"),
        aspect.join("sdk/proto/common.proto"),
    ];

    let includes = [root.join("bazel"), root.join("aspect")];

    let mut config = prost_build::Config::new();
    config.file_descriptor_set_path(out("descriptor.bin"));
    // Keep large generated code readable; needed messages are accessed via
    // the re-exported modules below.
    config.compile_protos(&bazel_protos, &includes).unwrap();
}

fn out(name: &str) -> PathBuf {
    PathBuf::from(std::env::var("OUT_DIR").unwrap()).join(name)
}