use std::path::PathBuf;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let manifest_dir = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR")?);
    let out_dir = PathBuf::from(std::env::var("OUT_DIR")?);

    let proto_root = manifest_dir.join("..").join("proto");
    let health_proto = proto_root.join("rmail").join("v1").join("health.proto");
    let descriptor_path = out_dir.join("rmail_descriptor.bin");

    tonic_build::configure()
        .file_descriptor_set_path(&descriptor_path)
        .compile_protos(&[health_proto.as_path()], &[proto_root.as_path()])?;

    // Rerun whenever any proto under the shared root changes, so adding a new
    // proto file later still triggers regeneration.
    println!("cargo:rerun-if-changed={}", proto_root.display());
    Ok(())
}
