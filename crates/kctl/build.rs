fn main() -> Result<(), Box<dyn std::error::Error>> {
    let manifest_dir = std::path::PathBuf::from(std::env::var("CARGO_MANIFEST_DIR")?);
    let proto_dir = manifest_dir.join("..").join("..").join("proto");
    let controller_proto = proto_dir.join("controller.proto");
    let node_proto = proto_dir.join("node.proto");

    tonic_build::configure()
        .build_server(false)
        .build_client(true)
        .compile_protos(&[controller_proto], std::slice::from_ref(&proto_dir))?;

    tonic_build::configure()
        .build_server(false)
        .build_client(true)
        .compile_protos(&[node_proto], std::slice::from_ref(&proto_dir))?;

    // One descriptor set over every service the agent may call by name. The agent builds
    // requests from JSON at run time, so it needs the schemas, not generated clients.
    let reflect_dir = std::path::PathBuf::from(std::env::var("OUT_DIR")?).join("reflect");
    std::fs::create_dir_all(&reflect_dir)?;
    tonic_build::configure()
        .build_server(false)
        .build_client(false)
        .out_dir(&reflect_dir)
        .file_descriptor_set_path(reflect_dir.join("kcore.bin"))
        .compile_protos(
            &[
                proto_dir.join("controller.proto"),
                proto_dir.join("node.proto"),
                proto_dir.join("provisioning.proto"),
            ],
            std::slice::from_ref(&proto_dir),
        )?;

    println!("cargo:rerun-if-changed={}", proto_dir.display());

    Ok(())
}
