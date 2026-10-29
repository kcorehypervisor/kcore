//! Install the bundled Slurm module on nodes that predate this release.

pub const SLURM_MODULE_NIX: &str = include_str!("../../../modules/kcore-slurm.nix");

pub fn install_slurm_module_sources() -> std::io::Result<()> {
    std::fs::create_dir_all("/etc/nixos/modules")?;
    std::fs::write("/etc/nixos/modules/kcore-slurm.nix", SLURM_MODULE_NIX)?;
    let config_path = "/etc/nixos/configuration.nix";
    let config = std::fs::read_to_string(config_path)?;
    let updated = crate::nixgen_k8s::ensure_nixos_imports(
        &config,
        &["./modules/kcore-slurm.nix", "./kcore-slurm.nix"],
    );
    if updated != config {
        std::fs::write(config_path, updated)?;
    }
    Ok(())
}
