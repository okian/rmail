use std::path::PathBuf;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let manifest_dir = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR")?);
    let out_dir = PathBuf::from(std::env::var("OUT_DIR")?);

    let proto_root = manifest_dir.join("..").join("proto");
    let v1 = proto_root.join("rmail").join("v1");
    // Every `.proto` under the versioned package directory is compiled. Listing
    // them by hand meant a new service could only be added by also editing this
    // file — a step to forget, and a point every concurrently developed service
    // has to serialize through. Sorting keeps the descriptor set (and therefore
    // reflection's ordering) byte-stable across machines, since readdir order
    // is not.
    let mut protos: Vec<PathBuf> = std::fs::read_dir(&v1)?
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.path())
        .filter(|path| path.extension().is_some_and(|ext| ext == "proto"))
        .collect();
    protos.sort();

    if protos.is_empty() {
        return Err(format!("no .proto files found under {}", v1.display()).into());
    }

    let descriptor_path = out_dir.join("rmail_descriptor.bin");

    tonic_build::configure()
        .file_descriptor_set_path(&descriptor_path)
        .compile_protos(&protos, &[proto_root.as_path()])?;

    // Rerun whenever any proto under the shared root changes, so adding a new
    // proto file later still triggers regeneration.
    println!("cargo:rerun-if-changed={}", proto_root.display());
    Ok(())
}
