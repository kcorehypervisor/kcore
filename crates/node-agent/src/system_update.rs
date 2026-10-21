//! Stage a kcore or NixOS pin as a NixOS generation, or as a versioned
//! symlink when the node has not learned the host flake yet.

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use serde::{Deserialize, Serialize};

pub const NIXOS_GENERATION: &str = "nixos-generation";
pub const HOST_FLAKE: &str = "/etc/nixos/flake.nix";
/// nixos-rebuild `--flake` name. This is the `nixosConfigurations` key, not a package.
pub const HOST_FLAKE_ATTR: &str = "/etc/nixos#kcore";
/// `nix build` attribute. `#kcore` is not a package output of the host flake.
pub const HOST_TOPLEVEL_ATTR: &str =
    "/etc/nixos#nixosConfigurations.kcore.config.system.build.toplevel";
pub const BOOT_PENDING: &str = "/var/lib/kcore/updates/boot-pending";

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PlannedActivation {
    /// Userspace only. The bootloader stays on this generation.
    Switch,
    /// Stage the generation and leave the running kernel in place.
    Boot,
}

pub fn classify_activation(vm_units_changed: bool, kernel_changed: bool) -> PlannedActivation {
    if vm_units_changed || kernel_changed {
        PlannedActivation::Boot
    } else {
        PlannedActivation::Switch
    }
}

pub fn validate_flake_rev(rev: &str) -> Result<(), String> {
    let rev = rev.trim();
    if rev.len() != 40 || !rev.chars().all(|c| c.is_ascii_hexdigit()) {
        return Err("flake_rev must be a 40-character git commit".into());
    }
    Ok(())
}

/// Drop imports the host module already provides so the two copies of ch-vm
/// are not evaluated together.
pub fn strip_legacy_imports(src: &str) -> String {
    src.lines()
        .filter(|line| {
            let t = line.trim();
            !(t.contains("./modules/ch-vm")
                || t.contains("./modules/kcore-branding.nix")
                || t.contains("./modules/kcore-ceph.nix")
                || t.contains("./modules/kcore-metrics.nix")
                || t.contains("./kcore-vms.nix")
                || t.contains("./kcore-ceph.nix")
                || t.contains("./kcore-clickhouse.nix")
                || t.contains("./hardware-configuration.nix"))
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Remove service blocks the host module owns. A normal Nix assignment would
/// conflict with the module's `ExecStart`.
pub fn strip_owned_service_blocks(src: &str) -> String {
    const NEEDLES: &[&str] = &[
        "systemd.services.kcore-node-agent",
        "systemd.services.kcore-controller",
        "systemd.services.kcore-dashboard",
        "systemd.services.kcore-br0-forward-delay",
        "system.activationScripts.kctl-path",
    ];
    let mut out = String::new();
    let mut i = 0;
    while i < src.len() {
        if let Some(needle) = NEEDLES.iter().find(|n| src[i..].starts_with(*n)) {
            if let Some(skip) = skip_nix_binding(&src[i + needle.len()..]) {
                i += needle.len() + skip;
                continue;
            }
        }
        let ch = src[i..].chars().next().unwrap();
        out.push(ch);
        i += ch.len_utf8();
    }
    out
}

fn skip_nix_binding(s: &str) -> Option<usize> {
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() && bytes[i].is_ascii_whitespace() {
        i += 1;
    }
    if i >= bytes.len() || bytes[i] != b'=' {
        return None;
    }
    i += 1;
    let mut depth = 0i32;
    let mut started = false;
    while i < bytes.len() {
        match bytes[i] {
            b'{' | b'[' | b'(' => {
                depth += 1;
                started = true;
            }
            b'}' | b']' | b')' => {
                if depth > 0 {
                    depth -= 1;
                }
            }
            b';' if started && depth == 0 => return Some(i + 1),
            _ => {}
        }
        i += 1;
    }
    None
}

pub fn vm_unit_names(paths: impl IntoIterator<Item = impl AsRef<str>>) -> BTreeSet<String> {
    paths
        .into_iter()
        .map(|p| p.as_ref().to_string())
        .filter(|p| p.contains("kcore-vm-"))
        .collect()
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct StagedManifest {
    pub kind: String,
    pub activation: String,
    pub reboot_required: bool,
    pub activated: bool,
    pub toplevel: String,
    pub current_generation: String,
    pub target_generation: String,
    pub previous_boot_id: String,
    pub version: String,
    pub manifest_path: String,
}

/// Point `bin_link` at `versions_root/<version>` after copying `bins` there.
/// The previous link target is stored in `versions_root/previous-target`.
pub fn stage_versioned_bins(
    versions_root: &Path,
    bin_link: &Path,
    version: &str,
    bins: &[(&str, &Path)],
) -> Result<(), String> {
    if version.is_empty() || version.contains('/') || version.contains("..") {
        return Err("version is not a single path segment".into());
    }
    let dest = versions_root.join(version);
    fs::create_dir_all(&dest).map_err(|e| format!("mkdir {}: {e}", dest.display()))?;
    for (name, src) in bins {
        if name.contains('/') || name.contains("..") {
            return Err(format!("refusing binary name {name}"));
        }
        let target = dest.join(name);
        fs::copy(src, &target).map_err(|e| format!("copy {}: {e}", target.display()))?;
    }
    if let Ok(previous) = fs::read_link(bin_link) {
        fs::write(
            versions_root.join("previous-target"),
            previous.display().to_string(),
        )
        .map_err(|e| format!("record previous target: {e}"))?;
    }
    if bin_link.exists() && !bin_link.is_symlink() {
        return Err(format!(
            "{} is a directory; refuse to replace it in place",
            bin_link.display()
        ));
    }
    if let Some(parent) = bin_link.parent() {
        fs::create_dir_all(parent).map_err(|e| format!("mkdir {}: {e}", parent.display()))?;
    }
    let _ = fs::remove_file(bin_link);
    std::os::unix::fs::symlink(&dest, bin_link)
        .map_err(|e| format!("symlink {}: {e}", bin_link.display()))?;
    Ok(())
}

pub fn rollback_versioned_bins(versions_root: &Path, bin_link: &Path) -> Result<String, String> {
    let previous_file = versions_root.join("previous-target");
    let previous = fs::read_to_string(&previous_file)
        .map_err(|e| format!("read {}: {e}", previous_file.display()))?;
    let previous = previous.trim();
    if previous.is_empty() {
        return Err("previous target is empty".into());
    }
    let _ = fs::remove_file(bin_link);
    std::os::unix::fs::symlink(previous, bin_link).map_err(|e| format!("restore symlink: {e}"))?;
    Ok(previous.to_string())
}

pub fn read_host_report() -> HostReport {
    let version = fs::read_to_string("/etc/kcore/version")
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| env!("CARGO_PKG_VERSION").to_string());
    HostReport {
        kcore_version: version,
        booted_generation: link_target("/run/booted-system"),
        nixos_generation: link_target("/run/current-system"),
        update_capabilities: vec![NIXOS_GENERATION.to_string()],
        schema_version: fs::read_to_string("/var/lib/kcore/schema-version")
            .ok()
            .and_then(|s| s.trim().parse().ok())
            .unwrap_or(0),
        nixpkgs_rev: fs::read_to_string("/etc/kcore/nixpkgs-rev")
            .map(|s| s.trim().to_string())
            .unwrap_or_default(),
    }
}

fn link_target(path: &str) -> String {
    fs::read_link(path)
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_default()
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostReport {
    pub kcore_version: String,
    pub booted_generation: String,
    pub nixos_generation: String,
    pub update_capabilities: Vec<String>,
    pub schema_version: i32,
    pub nixpkgs_rev: String,
}

pub fn flake_rebuild_suffix() -> String {
    if Path::new(HOST_FLAKE).exists() {
        format!("--flake {HOST_FLAKE_ATTR}")
    } else {
        String::new()
    }
}

pub fn write_manifest(path: &Path, manifest: &StagedManifest) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|e| format!("mkdir {}: {e}", parent.display()))?;
    }
    let body = serde_json::to_string_pretty(manifest).map_err(|e| e.to_string())?;
    fs::write(path, body).map_err(|e| format!("write {}: {e}", path.display()))?;
    Ok(())
}

pub fn read_manifest(path: &Path) -> Result<StagedManifest, String> {
    let body = fs::read_to_string(path).map_err(|e| format!("read {}: {e}", path.display()))?;
    serde_json::from_str(&body).map_err(|e| format!("decode {}: {e}", path.display()))
}

/// Build and stage. `nix` must be on `PATH`. Does not reboot.
pub fn prepare_generation(req: &PrepareRequest) -> Result<StagedManifest, String> {
    validate_flake_rev(&req.flake_rev)?;
    ensure_free_space("/nix/store", 4 * 1024 * 1024 * 1024)?;
    let update_dir = PathBuf::from("/var/lib/kcore/updates").join(&req.update_name);
    fs::create_dir_all(&update_dir).map_err(|e| e.to_string())?;
    if req.update_kind == "bootstrap" {
        return Err("bootstrap is staged with stage_versioned_bins, not a generation".into());
    }
    let had_flake = Path::new(HOST_FLAKE).exists();
    if !had_flake {
        write_migration_flake(&req.flake_ref, &req.flake_rev, &req.nixpkgs_rev)?;
    } else {
        rewrite_host_inputs(
            Path::new(HOST_FLAKE),
            &req.update_kind,
            &req.flake_ref,
            &req.flake_rev,
            &req.nixpkgs_rev,
        )?;
    }
    let toplevel = build_toplevel()?;
    let current = link_target("/run/current-system");
    let first_flake = !had_flake;
    let vm_changed = first_flake || vm_units_differ(&current, &toplevel);
    let kernel_changed = kernel_of(&current) != kernel_of(&toplevel);
    let plan = if first_flake {
        PlannedActivation::Boot
    } else {
        classify_activation(vm_changed, kernel_changed)
    };
    let (activation, reboot_required) = match plan {
        PlannedActivation::Switch => ("switch", false),
        PlannedActivation::Boot => ("boot", true),
    };
    let manifest = StagedManifest {
        kind: "generation".into(),
        activation: activation.into(),
        reboot_required,
        activated: false,
        toplevel: toplevel.clone(),
        current_generation: current,
        target_generation: toplevel,
        previous_boot_id: current_boot_id(),
        version: req.version.clone(),
        manifest_path: update_dir.join("manifest.json").display().to_string(),
    };
    write_manifest(&update_dir.join("manifest.json"), &manifest)?;
    Ok(manifest)
}

pub struct PrepareRequest {
    pub update_name: String,
    pub update_kind: String,
    pub flake_ref: String,
    pub flake_rev: String,
    pub nixpkgs_rev: String,
    pub version: String,
    pub had_flake: bool,
}

fn ensure_free_space(path: &str, min_bytes: u64) -> Result<(), String> {
    let out = Command::new("df")
        .args(["-B1", "--output=avail", path])
        .output()
        .map_err(|e| format!("df {path}: {e}"))?;
    if !out.status.success() {
        return Err(format!("df {path} failed"));
    }
    let text = String::from_utf8_lossy(&out.stdout);
    let avail = text
        .lines()
        .filter_map(|l| l.trim().parse::<u64>().ok())
        .next()
        .ok_or_else(|| format!("df {path} produced no size"))?;
    if avail < min_bytes {
        return Err(format!(
            "{path} has {avail} bytes free; need at least {min_bytes}"
        ));
    }
    Ok(())
}

fn build_toplevel() -> Result<String, String> {
    let out = Command::new("nix")
        .args([
            "build",
            HOST_TOPLEVEL_ATTR,
            "--no-link",
            "--print-out-paths",
        ])
        .output()
        .map_err(|e| format!("nix build: {e}"))?;
    if !out.status.success() {
        return Err(format!(
            "nix build failed: {}",
            String::from_utf8_lossy(&out.stderr)
        ));
    }
    let path = String::from_utf8_lossy(&out.stdout)
        .lines()
        .next()
        .unwrap_or("")
        .trim()
        .to_string();
    if path.is_empty() {
        return Err("nix build printed no store path".into());
    }
    Ok(path)
}

fn kernel_of(toplevel: &str) -> String {
    if toplevel.is_empty() {
        return String::new();
    }
    link_target(&format!("{toplevel}/kernel"))
}

fn vm_units_differ(current: &str, target: &str) -> bool {
    vm_unit_names(unit_paths(current)) != vm_unit_names(unit_paths(target))
}

fn unit_paths(toplevel: &str) -> Vec<String> {
    if toplevel.is_empty() {
        return Vec::new();
    }
    let dir = PathBuf::from(toplevel).join("etc/systemd/system");
    let Ok(entries) = fs::read_dir(dir) else {
        return Vec::new();
    };
    entries
        .filter_map(|e| e.ok())
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect()
}

fn current_boot_id() -> String {
    let out = Command::new("bootctl").arg("status").output();
    let Ok(out) = out else {
        return String::new();
    };
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .find_map(|l| l.trim().strip_prefix("id:").map(|s| s.trim().to_string()))
        .unwrap_or_default()
}

fn write_migration_flake(
    flake_ref: &str,
    flake_rev: &str,
    nixpkgs_rev: &str,
) -> Result<(), String> {
    let config = Path::new("/etc/nixos/configuration.nix");
    if config.exists() {
        let src = fs::read_to_string(config).map_err(|e| e.to_string())?;
        let stripped = strip_owned_service_blocks(&strip_legacy_imports(&src));
        fs::write("/etc/nixos/legacy-configuration.nix", stripped).map_err(|e| e.to_string())?;
    }
    let nixpkgs = if nixpkgs_rev.trim().len() == 40 {
        format!("github:NixOS/nixpkgs/{nixpkgs_rev}")
    } else {
        "github:NixOS/nixpkgs/nixos-unstable".into()
    };
    let kcore = format!("{flake_ref}/{flake_rev}");
    let flake = format!(
        r#"{{
  description = "kcore node";
  inputs = {{
    nixpkgs.url = "{nixpkgs}";
    kcore.url = "{kcore}";
  }};
  outputs = {{ self, nixpkgs, kcore, ... }}: {{
    nixosConfigurations.kcore = nixpkgs.lib.nixosSystem {{
      system = "x86_64-linux";
      specialArgs = {{ inherit kcore; }};
      modules = [
        kcore.nixosModules.kcore-host
        ({{ kcore, ... }}: {{
          services.kcore.nodeAgent.package = kcore.packages.x86_64-linux.kcore-node-agent;
          services.kcore.controller.package = kcore.packages.x86_64-linux.kcore-controller;
          services.kcore.dashboard.package = kcore.packages.x86_64-linux.kcore-dashboard;
          services.kcore.console.package = kcore.packages.x86_64-linux.kcore-console;
          services.kcore.kctl.package = kcore.packages.x86_64-linux.kctl;
          services.kcore.nodeAgent.enable = true;
          services.kcore.console.enable = true;
        }})
        ./hardware-configuration.nix
        ./kcore-vms.nix
        ./kcore-ceph.nix
        ./kcore-clickhouse.nix
        ./legacy-configuration.nix
      ];
    }};
    # The installed updater builds `/etc/nixos#kcore`. That is a package
    # attribute, not the nixosConfiguration name nixos-rebuild uses.
    packages.x86_64-linux.kcore = self.nixosConfigurations.kcore.config.system.build.toplevel;
  }};
}}
"#
    );
    fs::write(HOST_FLAKE, flake).map_err(|e| format!("write flake: {e}"))?;
    let values = Path::new("/etc/nixos/kcore-clickhouse.nix");
    if !values.exists() {
        fs::write(values, "{ ... }:\n{\n}\n")
            .map_err(|e| format!("write clickhouse values: {e}"))?;
    }
    Ok(())
}

pub fn rewrite_host_inputs(
    flake: &Path,
    update_kind: &str,
    flake_ref: &str,
    flake_rev: &str,
    nixpkgs_rev: &str,
) -> Result<(), String> {
    let mut text = fs::read_to_string(flake).map_err(|e| e.to_string())?;
    if update_kind != "nixpkgs" {
        let url = format!("{flake_ref}/{flake_rev}");
        text = replace_input_url(&text, "kcore", &url)?;
    }
    if update_kind == "nixpkgs" || !nixpkgs_rev.trim().is_empty() {
        validate_flake_rev(nixpkgs_rev)?;
        let url = format!("github:NixOS/nixpkgs/{nixpkgs_rev}");
        text = replace_input_url(&text, "nixpkgs", &url)?;
        let _ = fs::write("/etc/kcore/nixpkgs-rev", format!("{nixpkgs_rev}\n"));
    }
    fs::write(flake, text).map_err(|e| e.to_string())?;
    Ok(())
}

fn replace_input_url(text: &str, name: &str, url: &str) -> Result<String, String> {
    let marker = format!("{name}.url");
    let Some(at) = text.find(&marker) else {
        return Err(format!("host flake has no {marker}"));
    };
    let after = &text[at + marker.len()..];
    let Some(eq) = after.find('=') else {
        return Err(format!("{marker} has no value"));
    };
    let rest = &after[eq + 1..];
    let Some(start_rel) = rest.find('"') else {
        return Err(format!("{marker} is not a quoted url"));
    };
    let value = &rest[start_rel + 1..];
    let Some(end_rel) = value.find('"') else {
        return Err(format!("{marker} value is not closed"));
    };
    let abs_start = at + marker.len() + eq + 1 + start_rel + 1;
    let abs_end = abs_start + end_rel;
    let mut out = String::new();
    out.push_str(&text[..abs_start]);
    out.push_str(url);
    out.push_str(&text[abs_end..]);
    Ok(out)
}

pub struct ActivateOutcome {
    pub message: String,
    pub requires_reboot: bool,
    pub phase: String,
}

pub fn activate_staged(manifest_path: &Path, mode: &str) -> Result<ActivateOutcome, String> {
    let mut manifest = read_manifest(manifest_path)?;
    if manifest.kind == "bootstrap" {
        return Ok(ActivateOutcome {
            message: "bootstrap symlink is already current".into(),
            requires_reboot: false,
            phase: "succeeded".into(),
        });
    }
    let activation = if mode == "boot" || mode == "switch" {
        mode.to_string()
    } else {
        manifest.activation.clone()
    };
    let flake = flake_rebuild_suffix();
    let args = if flake.is_empty() {
        activation.clone()
    } else {
        format!("{activation} {flake}")
    };
    let status = Command::new("nixos-rebuild")
        .args(args.split_whitespace())
        .status()
        .map_err(|e| format!("nixos-rebuild: {e}"))?;
    if !status.success() {
        return Err(format!("nixos-rebuild {activation} failed"));
    }
    if activation == "boot" {
        if !manifest.previous_boot_id.is_empty() {
            let _ = Command::new("bootctl")
                .args(["set-default", &manifest.previous_boot_id])
                .status();
        }
        let _ = Command::new("bootctl")
            .args(["set-oneshot", "@current"])
            .status();
        let pending = format!(
            "version={}\ntoplevel={}\n",
            manifest.version, manifest.toplevel
        );
        if let Some(parent) = Path::new(BOOT_PENDING).parent() {
            let _ = fs::create_dir_all(parent);
        }
        fs::write(BOOT_PENDING, pending).map_err(|e| format!("boot-pending: {e}"))?;
    }
    manifest.activated = true;
    let reboot_required = manifest.reboot_required || activation == "boot";
    write_manifest(manifest_path, &manifest)?;
    if reboot_required {
        Ok(ActivateOutcome {
            message: "staged; reboot is waiting for kctl update reboot".into(),
            requires_reboot: true,
            phase: "reboot_required".into(),
        })
    } else {
        Ok(ActivateOutcome {
            message: "switched".into(),
            requires_reboot: false,
            phase: "succeeded".into(),
        })
    }
}

pub fn rollback_staged(manifest_path: &Path) -> Result<String, String> {
    if !manifest_path.exists() {
        return Ok("no staged manifest".into());
    }
    let manifest = read_manifest(manifest_path)?;
    if manifest.kind == "bootstrap" {
        return rollback_versioned_bins(
            Path::new("/opt/kcore/versions"),
            Path::new("/opt/kcore/bin"),
        );
    }
    let _ = fs::remove_file(BOOT_PENDING);
    if !manifest.previous_boot_id.is_empty() {
        let _ = Command::new("bootctl")
            .args(["set-default", &manifest.previous_boot_id])
            .status();
    }
    if manifest.activated && manifest.activation == "switch" {
        let flake = flake_rebuild_suffix();
        let mut cmd = Command::new("nixos-rebuild");
        cmd.arg("switch").arg("--rollback");
        if !flake.is_empty() {
            cmd.args(flake.split_whitespace());
        }
        let status = cmd.status().map_err(|e| format!("rollback: {e}"))?;
        if !status.success() {
            return Err("nixos-rebuild switch --rollback failed".into());
        }
    }
    Ok("rolled back".into())
}

pub fn reboot_staged() -> Result<String, String> {
    if !Path::new(BOOT_PENDING).exists() {
        return Err("no staged boot is pending".into());
    }
    let status = Command::new("systemctl")
        .arg("reboot")
        .status()
        .map_err(|e| format!("reboot: {e}"))?;
    if !status.success() {
        return Err("systemctl reboot failed".into());
    }
    Ok("rebooting".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classify_switch_only_when_units_and_kernel_stay() {
        assert_eq!(classify_activation(false, false), PlannedActivation::Switch);
        assert_eq!(classify_activation(true, false), PlannedActivation::Boot);
        assert_eq!(classify_activation(false, true), PlannedActivation::Boot);
    }

    #[test]
    fn flake_rev_must_be_40_hex_chars() {
        assert!(validate_flake_rev("0123456789abcdef0123456789abcdef01234567").is_ok());
        assert!(validate_flake_rev("nixos-unstable").is_err());
        assert!(validate_flake_rev("abc").is_err());
    }

    #[test]
    fn strip_removes_owned_imports_and_service_blocks() {
        let src = "{\n  imports = [\n    ./hardware-configuration.nix\n    ./modules/ch-vm\n    ./kcore-vms.nix\n  ];\n  networking.hostName = \"n\";\n  systemd.services.kcore-node-agent = {\n    serviceConfig.ExecStart = \"/opt/kcore/bin/kcore-node-agent\";\n  };\n  system.stateVersion = \"25.05\";\n}\n";
        let out = strip_owned_service_blocks(&strip_legacy_imports(src));
        assert!(!out.contains("ch-vm"));
        assert!(!out.contains("kcore-node-agent"));
        assert!(out.contains("hostName"));
        assert!(out.contains("stateVersion"));
    }

    #[test]
    fn versioned_symlink_round_trip() {
        let root = tempfile::tempdir().unwrap();
        let versions = root.path().join("versions");
        let link = root.path().join("bin");
        let src = root.path().join("agent");
        fs::write(&src, b"new").unwrap();
        let old = root.path().join("old");
        fs::create_dir_all(&old).unwrap();
        fs::write(old.join("kcore-node-agent"), b"old").unwrap();
        std::os::unix::fs::symlink(&old, &link).unwrap();
        stage_versioned_bins(
            &versions,
            &link,
            "0.5.0",
            &[("kcore-node-agent", src.as_path())],
        )
        .unwrap();
        assert_eq!(fs::read(link.join("kcore-node-agent")).unwrap(), b"new");
        let restored = rollback_versioned_bins(&versions, &link).unwrap();
        assert_eq!(restored, old.display().to_string());
        assert_eq!(fs::read_link(&link).unwrap(), old);
    }

    #[test]
    fn refuses_to_replace_a_real_bin_directory() {
        let root = tempfile::tempdir().unwrap();
        let versions = root.path().join("versions");
        let link = root.path().join("bin");
        fs::create_dir_all(&link).unwrap();
        let src = root.path().join("agent");
        fs::write(&src, b"new").unwrap();
        let err = stage_versioned_bins(
            &versions,
            &link,
            "0.5.0",
            &[("kcore-node-agent", src.as_path())],
        )
        .unwrap_err();
        assert!(err.contains("refuse"));
    }

    #[test]
    fn rewrite_replaces_only_the_named_input() {
        let root = tempfile::tempdir().unwrap();
        let flake = root.path().join("flake.nix");
        fs::write(
            &flake,
            "{\n  inputs.nixpkgs.url = \"github:NixOS/nixpkgs/aaa\";\n  inputs.kcore.url = \"github:org/kcore/bbb\";\n}\n",
        )
        .unwrap();
        let rev = "0123456789abcdef0123456789abcdef01234567";
        rewrite_host_inputs(&flake, "kcore", "github:org/kcore", rev, "").unwrap();
        let text = fs::read_to_string(&flake).unwrap();
        assert!(text.contains(&format!("github:org/kcore/{rev}")));
        assert!(text.contains("github:NixOS/nixpkgs/aaa"));
    }
}
