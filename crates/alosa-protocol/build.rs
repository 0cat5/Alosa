// Compiles the .proto schema into Rust types at build time.
// protox is a pure-Rust protobuf compiler, so no system `protoc` is needed.
use std::path::PathBuf;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let proto_root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../protocol");
    let proto_file = proto_root.join("alosa/device/v1/device.proto");

    println!("cargo:rerun-if-changed={}", proto_file.display());

    let fds = protox::compile([&proto_file], [&proto_root])?;
    prost_build::Config::new().compile_fds(fds)?;
    Ok(())
}
