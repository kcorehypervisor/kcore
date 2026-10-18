fn main() -> Result<(), Box<dyn std::error::Error>> {
    let manifest_dir = std::path::PathBuf::from(std::env::var("CARGO_MANIFEST_DIR")?);
    let proto_dir = manifest_dir.join("..").join("..").join("proto");
    let controller_proto = proto_dir.join("controller.proto");

    tonic_build::configure()
        .build_server(true)
        .build_client(true)
        .compile_protos(
            std::slice::from_ref(&controller_proto),
            std::slice::from_ref(&proto_dir),
        )?;

    println!("cargo:rerun-if-changed={}", controller_proto.display());
    println!("cargo:rustc-check-cfg=cfg(dashboard_spa)");

    let spa = manifest_dir.join("web").join("dist").join("index.html");
    println!("cargo:rerun-if-changed={}", spa.display());
    if spa.is_file() {
        println!("cargo:rustc-cfg=dashboard_spa");
    }
    Ok(())
}
