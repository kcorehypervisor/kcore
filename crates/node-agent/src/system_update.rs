//! Stage a kcore or NixOS pin as a NixOS generation, or as a versioned
//! symlink when the node has not learned the host flake yet.
//!
//! Prepare builds a copy of `/etc/nixos` under the update directory. The live
//! host flake is only changed when the update is activated: routine
//! `nixos-rebuild switch` runs (VM, Ceph, Kubernetes config) keep building the
//! pins the node is running until then.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use serde::{Deserialize, Serialize};

pub const NIXOS_GENERATION: &str = "nixos-generation";
pub const HOST_DIR: &str = "/etc/nixos";
pub const HOST_FLAKE: &str = "/etc/nixos/flake.nix";
/// nixos-rebuild `--flake` name. This is the `nixosConfigurations` key, not a package.
pub const HOST_FLAKE_ATTR: &str = "/etc/nixos#kcore";
pub const UPDATES_DIR: &str = "/var/lib/kcore/updates";
pub const BOOT_PENDING: &str = "/var/lib/kcore/updates/boot-pending";
/// Written by kcore-boot-confirm when a staged generation did not come up
/// healthy and the node booted the previous one.
pub const BOOT_FAILED: &str = "/var/lib/kcore/updates/boot-failed";
/// A live switch waiting for the new node agent to confirm it.
pub const SWITCH_PENDING: &str = "/var/lib/kcore/updates/switch-pending";
pub const SWITCH_FAILED: &str = "/var/lib/kcore/updates/switch-failed";
/// Transient timer that undoes a switch nobody confirmed.
const GUARD_UNIT: &str = "kcore-update-guard";
const GUARD_SECONDS: u64 = 900;
/// The host flake every node runs. Shared with the ISO installer.
const HOST_FLAKE_TEMPLATE: &str = include_str!("../../../nix/host-flake.nix");
const SYSTEM_PROFILE: &str = "/nix/var/nix/profiles/system";
const TOPLEVEL_ATTR: &str = "nixosConfigurations.kcore.config.system.build.toplevel";

/// Host flake files an update replaces. The kcore boot-confirm unit in
/// `modules/kcore-host.nix` installs the same list after a staged boot.
pub const CANDIDATE_FILES: &[&str] = &["flake.nix", "flake.lock", "legacy-configuration.nix"];
/// Never copied into the staged host tree. `kcore-src` is an offline kcore
/// checkout that the host flake names by absolute path.
const STAGE_SKIP: &[&str] = &[".git", "kcore-src"];

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

/// Release tags are `vX.Y.Z`.
pub fn validate_release_tag(tag: &str) -> Result<(), String> {
    let rest = tag.strip_prefix('v').unwrap_or("");
    let parts: Vec<&str> = rest.split('.').collect();
    if parts.len() != 3
        || parts
            .iter()
            .any(|p| p.is_empty() || !p.chars().all(|c| c.is_ascii_digit()))
    {
        return Err(format!("{tag} is not a release tag like v1.2.3"));
    }
    Ok(())
}

/// `flake_ref` at a commit or tag. `github:`, `gitlab:`, and `sourcehut:`
/// take it as a path segment; `git+...` URLs take `rev=` or `ref=`.
pub fn flake_url_at(flake_ref: &str, at: &str) -> String {
    let flake_ref = flake_ref.trim().trim_end_matches('/');
    if ["github:", "gitlab:", "sourcehut:"]
        .iter()
        .any(|scheme| flake_ref.starts_with(scheme))
    {
        return format!("{flake_ref}/{at}");
    }
    let param = if validate_flake_rev(at).is_ok() {
        format!("rev={at}")
    } else {
        format!("ref=refs/tags/{at}")
    };
    let sep = if flake_ref.contains('?') { '&' } else { '?' };
    format!("{flake_ref}{sep}{param}")
}

pub fn render_host_flake(kcore_url: &str) -> String {
    HOST_FLAKE_TEMPLATE.replace("@KCORE_URL@", kcore_url)
}

/// The commit a host `flake.lock` pins for one of its root inputs.
pub fn locked_input_rev(lock: &str, input: &str) -> Option<String> {
    let lock: serde_json::Value = serde_json::from_str(lock).ok()?;
    let nodes = lock.get("nodes")?;
    let root = lock.get("root")?.as_str()?;
    let node = resolve_lock_input(nodes, root, root, &[input])?;
    Some(
        nodes
            .get(&node)?
            .get("locked")?
            .get("rev")?
            .as_str()?
            .to_string(),
    )
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

/// Drop a one-line `services.kcore.version = ...;` from a host flake. Older
/// installers wrote the install-time version there, which then outlived every
/// update. The kcore host module now sets it from the kcore input.
pub fn strip_version_pin(src: &str) -> String {
    let mut out: Vec<&str> = Vec::new();
    for line in src.lines() {
        let t = line.trim();
        if t.starts_with("services.kcore.version") && t.ends_with(';') {
            continue;
        }
        out.push(line);
    }
    let mut text = out.join("\n");
    if src.ends_with('\n') {
        text.push('\n');
    }
    text
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

/// `kcore-vm-*` unit files (and drop-ins) of a system closure, keyed by name,
/// valued by the store path they resolve to. A changed value means
/// switch-to-configuration would restart that guest.
pub fn vm_unit_fingerprints(toplevel: &Path) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    let dir = toplevel.join("etc/systemd/system");
    let Ok(entries) = fs::read_dir(&dir) else {
        return out;
    };
    for entry in entries.filter_map(|e| e.ok()) {
        let name = entry.file_name().to_string_lossy().into_owned();
        if !name.contains("kcore-vm-") {
            continue;
        }
        let path = entry.path();
        if path.is_dir() {
            if let Ok(children) = fs::read_dir(&path) {
                for child in children.filter_map(|e| e.ok()) {
                    let key = format!("{name}/{}", child.file_name().to_string_lossy());
                    out.insert(key, resolved(&child.path()));
                }
            }
        } else {
            out.insert(name, resolved(&path));
        }
    }
    out
}

fn resolved(path: &Path) -> String {
    fs::canonicalize(path)
        .map(|p| p.display().to_string())
        .unwrap_or_default()
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
    /// Copy of `/etc/nixos` with the target pins. Empty in manifests written
    /// before staging moved out of the live host flake.
    #[serde(default)]
    pub host_staging: String,
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
    let nixpkgs_rev = fs::read_to_string(Path::new(HOST_DIR).join("flake.lock"))
        .ok()
        .and_then(|lock| locked_nixpkgs_rev(&lock))
        .or_else(|| {
            fs::read_to_string("/etc/kcore/nixpkgs-rev")
                .ok()
                .map(|s| s.trim().to_string())
        })
        .unwrap_or_default();
    HostReport {
        kcore_version: version,
        booted_generation: link_target("/run/booted-system"),
        nixos_generation: link_target("/run/current-system"),
        update_capabilities: vec![NIXOS_GENERATION.to_string()],
        schema_version: fs::read_to_string("/var/lib/kcore/schema-version")
            .ok()
            .and_then(|s| s.trim().parse().ok())
            .unwrap_or(0),
        nixpkgs_rev,
    }
}

/// The nixpkgs commit a host `flake.lock` builds the OS from. The host flake
/// either has its own `nixpkgs` input or takes it from its `kcore` input.
/// `path:` inputs written by the ISO installer carry no rev.
pub fn locked_nixpkgs_rev(lock: &str) -> Option<String> {
    let lock: serde_json::Value = serde_json::from_str(lock).ok()?;
    let nodes = lock.get("nodes")?;
    let root = lock.get("root")?.as_str()?;
    let node = resolve_lock_input(nodes, root, root, &["nixpkgs"])
        .or_else(|| resolve_lock_input(nodes, root, root, &["kcore", "nixpkgs"]))?;
    let rev = nodes.get(&node)?.get("locked")?.get("rev")?.as_str()?;
    validate_flake_rev(rev).ok()?;
    Some(rev.to_string())
}

fn resolve_lock_input(
    nodes: &serde_json::Value,
    root: &str,
    from: &str,
    path: &[&str],
) -> Option<String> {
    let mut current = from.to_string();
    for name in path {
        let input = nodes.get(&current)?.get("inputs")?.get(*name)?;
        current = match input {
            serde_json::Value::String(node) => node.clone(),
            // `follows`: a path of input names from the root node.
            serde_json::Value::Array(follows) => {
                let names: Vec<&str> = follows.iter().filter_map(|v| v.as_str()).collect();
                resolve_lock_input(nodes, root, root, &names)?
            }
            _ => return None,
        };
    }
    Some(current)
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

/// Copy `src` into a fresh `dst`, skipping [`STAGE_SKIP`], `result*` links,
/// and the top-level names in `except`. Symlinks are copied as the files they
/// point at, because a `path:` flake cannot follow links out of its tree.
pub fn copy_host_tree(src: &Path, dst: &Path, except: &[&str]) -> Result<(), String> {
    if dst.exists() {
        fs::remove_dir_all(dst).map_err(|e| format!("clear {}: {e}", dst.display()))?;
    }
    fs::create_dir_all(dst).map_err(|e| format!("mkdir {}: {e}", dst.display()))?;
    let entries = fs::read_dir(src).map_err(|e| format!("read {}: {e}", src.display()))?;
    for entry in entries.filter_map(|e| e.ok()) {
        let name = entry.file_name().to_string_lossy().into_owned();
        if STAGE_SKIP.contains(&name.as_str())
            || name.starts_with("result")
            || except.contains(&name.as_str())
        {
            continue;
        }
        copy_entry(&entry.path(), &dst.join(&name))?;
    }
    Ok(())
}

fn copy_entry(src: &Path, dst: &Path) -> Result<(), String> {
    let Ok(meta) = fs::metadata(src) else {
        // Dangling link: nothing the flake could have imported.
        return Ok(());
    };
    if meta.is_dir() {
        fs::create_dir_all(dst).map_err(|e| format!("mkdir {}: {e}", dst.display()))?;
        let entries = fs::read_dir(src).map_err(|e| format!("read {}: {e}", src.display()))?;
        for entry in entries.filter_map(|e| e.ok()) {
            copy_entry(&entry.path(), &dst.join(entry.file_name()))?;
        }
    } else if meta.is_file() {
        fs::copy(src, dst).map_err(|e| format!("copy {}: {e}", src.display()))?;
    }
    Ok(())
}

/// Refresh the staged tree from the live host so node-local config written
/// since prepare (VMs, Ceph, Kubernetes) is part of the activated generation.
/// The staged [`CANDIDATE_FILES`] are kept.
pub fn restage_host_tree(live: &Path, staging: &Path) -> Result<(), String> {
    let fresh = staging.with_extension("next");
    copy_host_tree(live, &fresh, CANDIDATE_FILES)?;
    for name in CANDIDATE_FILES {
        let staged = staging.join(name);
        if staged.is_file() {
            fs::copy(&staged, fresh.join(name)).map_err(|e| format!("keep staged {name}: {e}"))?;
        }
    }
    fs::remove_dir_all(staging).map_err(|e| format!("clear {}: {e}", staging.display()))?;
    fs::rename(&fresh, staging).map_err(|e| format!("rename staged tree: {e}"))?;
    Ok(())
}

/// Install the staged [`CANDIDATE_FILES`] into `live`. The files they replace
/// are kept in `previous` the first time, so a retried activation does not
/// overwrite the backup with the target pins.
pub fn install_candidate(live: &Path, staging: &Path, previous: &Path) -> Result<(), String> {
    if !previous.exists() {
        fs::create_dir_all(previous).map_err(|e| format!("mkdir {}: {e}", previous.display()))?;
        for name in CANDIDATE_FILES {
            let current = live.join(name);
            if current.is_file() {
                fs::copy(&current, previous.join(name))
                    .map_err(|e| format!("back up {name}: {e}"))?;
            } else {
                fs::write(previous.join(format!("{name}.absent")), b"")
                    .map_err(|e| format!("record absent {name}: {e}"))?;
            }
        }
    }
    for name in CANDIDATE_FILES {
        let staged = staging.join(name);
        if !staged.is_file() {
            continue;
        }
        let tmp = live.join(format!(".{name}.kcore-tmp"));
        fs::copy(&staged, &tmp).map_err(|e| format!("stage {name}: {e}"))?;
        fs::rename(&tmp, live.join(name)).map_err(|e| format!("install {name}: {e}"))?;
    }
    Ok(())
}

/// Put back the files [`install_candidate`] replaced. Returns false when
/// nothing was installed.
pub fn restore_previous(live: &Path, previous: &Path) -> Result<bool, String> {
    if !previous.is_dir() {
        return Ok(false);
    }
    for name in CANDIDATE_FILES {
        let saved = previous.join(name);
        if saved.is_file() {
            let tmp = live.join(format!(".{name}.kcore-tmp"));
            fs::copy(&saved, &tmp).map_err(|e| format!("restore {name}: {e}"))?;
            fs::rename(&tmp, live.join(name)).map_err(|e| format!("restore {name}: {e}"))?;
        } else if previous.join(format!("{name}.absent")).exists() {
            let _ = fs::remove_file(live.join(name));
        }
    }
    fs::remove_dir_all(previous).map_err(|e| format!("clear {}: {e}", previous.display()))?;
    Ok(true)
}

/// Build and stage. `nix` must be on `PATH`. Does not touch `/etc/nixos` and
/// does not reboot.
pub fn prepare_generation(req: &PrepareRequest) -> Result<StagedManifest, String> {
    validate_flake_rev(&req.flake_rev)?;
    if req.update_kind == "bootstrap" {
        return Err("bootstrap is staged with stage_versioned_bins, not a generation".into());
    }
    ensure_free_space("/nix/store", 4 * 1024 * 1024 * 1024)?;
    let update_dir = PathBuf::from(UPDATES_DIR).join(&req.update_name);
    fs::create_dir_all(&update_dir).map_err(|e| e.to_string())?;
    let staging = update_dir.join("host");
    copy_host_tree(Path::new(HOST_DIR), &staging, &[])?;
    let staged_flake = staging.join("flake.nix");
    let first_flake = !staged_flake.exists();
    if !req.flake_tag.is_empty() {
        validate_release_tag(&req.flake_tag)?;
        stage_release_flake(&staging, &flake_url_at(&req.flake_ref, &req.flake_tag))?;
    } else if first_flake {
        write_migration_flake(&staging, &req.flake_ref, &req.flake_rev, &req.nixpkgs_rev)?;
    } else {
        rewrite_host_inputs(
            &staged_flake,
            &req.update_kind,
            &req.flake_ref,
            &req.flake_rev,
            &req.nixpkgs_rev,
        )?;
        let text = fs::read_to_string(&staged_flake).map_err(|e| e.to_string())?;
        fs::write(&staged_flake, strip_version_pin(&text)).map_err(|e| e.to_string())?;
    }
    let toplevel = build_toplevel(&staging)?;
    if !req.flake_tag.is_empty() {
        let lock = fs::read_to_string(staging.join("flake.lock")).unwrap_or_default();
        let locked = locked_input_rev(&lock, "kcore").unwrap_or_default();
        if locked != req.flake_rev.trim() {
            return Err(format!(
                "{} resolves to {locked} on this node; the controller resolved {}",
                req.flake_tag,
                req.flake_rev.trim()
            ));
        }
    }
    let current = link_target("/run/current-system");
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
        previous_boot_id: default_boot_entry(),
        version: req.version.clone(),
        manifest_path: update_dir.join("manifest.json").display().to_string(),
        host_staging: staging.display().to_string(),
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
    /// Release tag. When set, the staged host flake is the shared template
    /// pointing at this tag, and the lock must pin `flake_rev`.
    pub flake_tag: String,
}

/// Replace the staged host flake with the release template. The old lock is
/// dropped so the new one is a function of the tag alone, like the lock the
/// ISO installs. A node without a host flake keeps its `configuration.nix`
/// as `legacy-configuration.nix`.
fn stage_release_flake(staging: &Path, kcore_url: &str) -> Result<(), String> {
    let config = staging.join("configuration.nix");
    if !staging.join("flake.nix").exists() && config.exists() {
        let src = fs::read_to_string(&config).map_err(|e| e.to_string())?;
        let stripped = strip_owned_service_blocks(&strip_legacy_imports(&src));
        fs::write(staging.join("legacy-configuration.nix"), stripped).map_err(|e| e.to_string())?;
    }
    fs::write(staging.join("flake.nix"), render_host_flake(kcore_url))
        .map_err(|e| format!("write flake: {e}"))?;
    let _ = fs::remove_file(staging.join("flake.lock"));
    Ok(())
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

fn staged_flake_ref(staging: &Path) -> String {
    format!("path:{}", staging.display())
}

/// Build the staged host flake. Writes its `flake.lock`, which pins every
/// input, including the kcore input's own nixpkgs, crane, and rust-overlay.
fn build_toplevel(staging: &Path) -> Result<String, String> {
    let out = Command::new("nix")
        .args([
            "build",
            &format!("{}#{TOPLEVEL_ATTR}", staged_flake_ref(staging)),
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
    if current.is_empty() {
        return true;
    }
    vm_unit_fingerprints(Path::new(current)) != vm_unit_fingerprints(Path::new(target))
}

/// The systemd-boot default entry id, or empty when systemd-boot is not the
/// boot loader.
fn default_boot_entry() -> String {
    if let Ok(out) = Command::new("bootctl")
        .args(["list", "--json=short"])
        .output()
    {
        if out.status.success() {
            if let Ok(serde_json::Value::Array(entries)) =
                serde_json::from_slice::<serde_json::Value>(&out.stdout)
            {
                if let Some(id) = entries
                    .iter()
                    .find(|e| e.get("isDefault").and_then(|v| v.as_bool()) == Some(true))
                    .and_then(|e| e.get("id"))
                    .and_then(|v| v.as_str())
                {
                    return id.to_string();
                }
            }
        }
    }
    let Ok(out) = Command::new("bootctl").arg("status").output() else {
        return String::new();
    };
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .find_map(|l| l.trim().strip_prefix("id:").map(|s| s.trim().to_string()))
        .unwrap_or_default()
}

/// After a generation became the boot default: keep `previous` as the default
/// and boot the new entry once. A generation that never gets blessed by
/// kcore-boot-confirm falls back to `previous` on the next reboot.
fn arm_oneshot(previous: &str) {
    let staged = default_boot_entry();
    if previous.is_empty() || staged.is_empty() || staged == previous {
        return;
    }
    let _ = Command::new("bootctl")
        .args(["set-default", previous])
        .status();
    let _ = Command::new("bootctl")
        .args(["set-oneshot", &staged])
        .status();
}

fn write_migration_flake(
    dir: &Path,
    flake_ref: &str,
    flake_rev: &str,
    nixpkgs_rev: &str,
) -> Result<(), String> {
    let config = dir.join("configuration.nix");
    if config.exists() {
        let src = fs::read_to_string(&config).map_err(|e| e.to_string())?;
        let stripped = strip_owned_service_blocks(&strip_legacy_imports(&src));
        fs::write(dir.join("legacy-configuration.nix"), stripped).map_err(|e| e.to_string())?;
    }
    fs::write(
        dir.join("flake.nix"),
        render_migration_flake(flake_ref, flake_rev, nixpkgs_rev),
    )
    .map_err(|e| format!("write flake: {e}"))
}

fn render_migration_flake(flake_ref: &str, flake_rev: &str, nixpkgs_rev: &str) -> String {
    // Without a tested OS pin the OS follows the nixpkgs the kcore commit
    // locks, so a single kcore rev pins the whole system.
    let nixpkgs = if validate_flake_rev(nixpkgs_rev).is_ok() {
        format!(
            "nixpkgs.url = \"github:NixOS/nixpkgs/{}\";",
            nixpkgs_rev.trim()
        )
    } else {
        "nixpkgs.follows = \"kcore/nixpkgs\";".into()
    };
    let kcore = flake_url_at(flake_ref, flake_rev);
    format!(
        r#"{{
  description = "kcore node";
  inputs = {{
    kcore.url = "{kcore}";
    {nixpkgs}
  }};
  outputs = {{ nixpkgs, kcore, ... }}: {{
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
        ./legacy-configuration.nix
      ];
    }};
  }};
}}
"#
    )
}

/// Point the staged host flake at the target pins. A host flake that takes
/// nixpkgs from its kcore input (kcore-infect, migration) moves the OS with
/// the kcore rev and has no separate nixpkgs pin to move.
pub fn rewrite_host_inputs(
    flake: &Path,
    update_kind: &str,
    flake_ref: &str,
    flake_rev: &str,
    nixpkgs_rev: &str,
) -> Result<(), String> {
    let mut text = fs::read_to_string(flake).map_err(|e| e.to_string())?;
    let own_nixpkgs = text.contains("nixpkgs.url");
    if update_kind != "nixpkgs" {
        text = replace_input_url(&text, "kcore", &flake_url_at(flake_ref, flake_rev))?;
    }
    let move_nixpkgs = if update_kind == "nixpkgs" {
        if !own_nixpkgs {
            return Err(
                "this node builds the OS from the nixpkgs its kcore input locks; move the kcore rev instead"
                    .into(),
            );
        }
        true
    } else {
        own_nixpkgs && !nixpkgs_rev.trim().is_empty()
    };
    if move_nixpkgs {
        validate_flake_rev(nixpkgs_rev)?;
        let url = format!("github:NixOS/nixpkgs/{}", nixpkgs_rev.trim());
        text = replace_input_url(&text, "nixpkgs", &url)?;
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
    pub target_generation: String,
}

fn nixos_rebuild(action: &str, flake: &str) -> Result<(), String> {
    let mut cmd = Command::new("nixos-rebuild");
    cmd.arg(action);
    if !flake.is_empty() {
        cmd.args(["--flake", flake]);
    }
    let status = cmd
        .status()
        .map_err(|e| format!("nixos-rebuild {action}: {e}"))?;
    if !status.success() {
        return Err(format!("nixos-rebuild {action} failed"));
    }
    Ok(())
}

fn update_dir_of(manifest_path: &Path) -> PathBuf {
    manifest_path
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from(UPDATES_DIR))
}

fn boot_pending_body(toplevel: &str, previous: &str, staging: &str, boot_id: &str) -> String {
    format!("toplevel={toplevel}\nprevious={previous}\nstaging={staging}\nboot_id={boot_id}\n")
}

fn boot_id() -> String {
    fs::read_to_string("/proc/sys/kernel/random/boot_id")
        .map(|s| s.trim().to_string())
        .unwrap_or_default()
}

/// `switch` installs the target pins into `/etc/nixos` before switching, so a
/// switch that restarts the agent still leaves the host flake on the running
/// pins. `boot` leaves `/etc/nixos` alone: kcore-boot-confirm installs the
/// pins once the staged generation has booted and been blessed.
pub fn activate_staged(manifest_path: &Path, mode: &str) -> Result<ActivateOutcome, String> {
    let mut manifest = read_manifest(manifest_path)?;
    if manifest.kind == "bootstrap" {
        return Ok(ActivateOutcome {
            message: "bootstrap symlink is already current".into(),
            requires_reboot: false,
            phase: "succeeded".into(),
            target_generation: String::new(),
        });
    }
    let activation = if mode == "boot" || mode == "switch" {
        mode.to_string()
    } else {
        manifest.activation.clone()
    };
    let update_dir = update_dir_of(manifest_path);
    let staging = if manifest.host_staging.is_empty() {
        update_dir.join("host")
    } else {
        PathBuf::from(&manifest.host_staging)
    };
    if !staging.join("flake.nix").is_file() {
        return Err("this update has no staged host flake; prepare it again".into());
    }
    let live = Path::new(HOST_DIR);
    restage_host_tree(live, &staging)?;
    let toplevel = build_toplevel(&staging)?;
    manifest.toplevel = toplevel.clone();
    manifest.target_generation = toplevel.clone();
    manifest.activation = activation.clone();
    write_manifest(manifest_path, &manifest)?;
    let staged_flake = format!("{}#kcore", staged_flake_ref(&staging));
    let running = resolved(Path::new("/run/current-system"));
    for marker in [BOOT_FAILED, SWITCH_FAILED] {
        let _ = fs::remove_file(marker);
    }

    if activation == "boot" {
        let previous = default_boot_entry();
        nixos_rebuild("boot", &staged_flake)?;
        arm_oneshot(&previous);
        if !previous.is_empty() {
            manifest.previous_boot_id = previous;
        }
        if let Some(parent) = Path::new(BOOT_PENDING).parent() {
            let _ = fs::create_dir_all(parent);
        }
        fs::write(
            BOOT_PENDING,
            boot_pending_body(
                &toplevel,
                &running,
                &staging.display().to_string(),
                &boot_id(),
            ),
        )
        .map_err(|e| format!("boot-pending: {e}"))?;
    } else {
        let previous_files = update_dir.join("previous");
        install_candidate(live, &staging, &previous_files)?;
        arm_switch_guard(&toplevel, &running, &update_dir)?;
        if let Err(e) = nixos_rebuild("switch", &staged_flake) {
            let reverted = revert_switch(&running, &update_dir, &e);
            return Err(match reverted {
                Ok(()) => format!("{e}; reverted to {running}"),
                Err(r) => format!("{e}; revert failed: {r}"),
            });
        }
        // The agent survived the switch: its package did not change.
        confirm_switch();
    }
    manifest.activated = true;
    write_manifest(manifest_path, &manifest)?;
    if activation == "boot" {
        Ok(ActivateOutcome {
            message: "staged; reboot is waiting for kctl update reboot".into(),
            requires_reboot: true,
            phase: "reboot_required".into(),
            target_generation: toplevel,
        })
    } else {
        Ok(ActivateOutcome {
            message: "switched".into(),
            requires_reboot: false,
            phase: "succeeded".into(),
            target_generation: toplevel,
        })
    }
}

/// What [`rollback_staged`] answers when the node still runs the update
/// generation and only leaves it at the next boot.
pub const ROLLED_BACK_REBOOT: &str = "rolled back; reboot the node to leave the update generation";

/// Put the host flake back on the pins it had before the update and rebuild
/// from it. Generations are not deleted.
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
    let pending = fs::read_to_string(BOOT_PENDING).unwrap_or_default();
    if !manifest.toplevel.is_empty() && pending.contains(&manifest.toplevel) {
        let _ = fs::remove_file(BOOT_PENDING);
    }
    disarm_switch_guard();
    let _ = fs::remove_file(SWITCH_PENDING);
    let live = Path::new(HOST_DIR);
    let restored = restore_previous(live, &update_dir_of(manifest_path).join("previous"))?;
    if !restored && !manifest.activated {
        return Ok("nothing was activated; /etc/nixos was not changed".into());
    }
    let booted_target = !manifest.toplevel.is_empty()
        && resolved(Path::new("/run/booted-system")) == manifest.toplevel;
    let flake = if Path::new(HOST_FLAKE).exists() {
        HOST_FLAKE_ATTR
    } else {
        ""
    };
    if manifest.activation == "boot" || booted_target {
        let _ = Command::new("bootctl").args(["set-oneshot", ""]).status();
        if !booted_target && !manifest.previous_boot_id.is_empty() {
            let _ = Command::new("bootctl")
                .args(["set-default", &manifest.previous_boot_id])
                .status();
        }
        nixos_rebuild("boot", flake)?;
        if booted_target {
            return Ok(ROLLED_BACK_REBOOT.into());
        }
        return Ok("rolled back; the staged generation will not boot".into());
    }
    nixos_rebuild("switch", flake)?;
    Ok("rolled back".into())
}

/// Reboot into the staged generation. A routine `nixos-rebuild switch` after
/// activation (VM or Ceph config) makes a newer generation the boot default,
/// so the staged one is made the default again first.
pub fn reboot_staged() -> Result<String, String> {
    let pending = fs::read_to_string(BOOT_PENDING).map_err(|_| "no staged boot is pending")?;
    let toplevel = pending
        .lines()
        .find_map(|l| l.strip_prefix("toplevel="))
        .unwrap_or("")
        .trim()
        .to_string();
    if !toplevel.is_empty() && resolved(Path::new(SYSTEM_PROFILE)) != toplevel {
        let previous = default_boot_entry();
        let status = Command::new("nix-env")
            .args(["-p", SYSTEM_PROFILE, "--set", &toplevel])
            .status()
            .map_err(|e| format!("nix-env --set: {e}"))?;
        if !status.success() {
            return Err("could not make the staged generation the system profile".into());
        }
        let status = Command::new(format!("{toplevel}/bin/switch-to-configuration"))
            .arg("boot")
            .status()
            .map_err(|e| format!("switch-to-configuration boot: {e}"))?;
        if !status.success() {
            return Err("switch-to-configuration boot failed".into());
        }
        arm_oneshot(&previous);
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

/// Phase, target closure, and failure reason for GetSystemUpdateStatus.
/// `failed` means the node already went back to the previous generation.
/// `boot_pending` means a staged boot has not been confirmed healthy yet.
pub fn update_status(manifest_path: &Path) -> (String, String, String) {
    let Ok(m) = read_manifest(manifest_path) else {
        return ("unknown".into(), String::new(), String::new());
    };
    for marker in [BOOT_FAILED, SWITCH_FAILED] {
        let body = fs::read_to_string(marker).unwrap_or_default();
        if marker_field(&body, "toplevel") == Some(m.toplevel.as_str()) && !m.toplevel.is_empty() {
            let reason = marker_field(&body, "reason").unwrap_or("").to_string();
            return ("failed".into(), m.target_generation, reason);
        }
    }
    let pending = fs::read_to_string(BOOT_PENDING).unwrap_or_default();
    let phase = if !m.toplevel.is_empty()
        && marker_field(&pending, "toplevel") == Some(m.toplevel.as_str())
    {
        "boot_pending"
    } else if m.activated {
        "activated"
    } else {
        "prepared"
    };
    (phase.into(), m.target_generation, String::new())
}

fn marker_field<'a>(body: &'a str, key: &str) -> Option<&'a str> {
    body.lines()
        .find_map(|l| l.strip_prefix(key).and_then(|r| r.strip_prefix('=')))
        .map(str::trim)
}

/// Shell for the guard timer. It runs from PID 1, so it outlives the agent
/// and the switch, and it only uses the generation that was running before.
fn guard_script(previous: &str, update_dir: &Path) -> String {
    let dir = update_dir.display();
    format!(
        r#"set -u
m={SWITCH_PENDING}
[ -f "$m" ] || exit 0
target=$(sed -n 's/^toplevel=//p' "$m")
rm -f "$m"
for f in {files}; do
  if [ -f "{dir}/previous/$f" ]; then
    cp "{dir}/previous/$f" "{HOST_DIR}/.$f.kcore-tmp" && mv -f "{HOST_DIR}/.$f.kcore-tmp" "{HOST_DIR}/$f"
  elif [ -e "{dir}/previous/$f.absent" ]; then
    rm -f "{HOST_DIR}/$f"
  fi
done
rm -rf "{dir}/previous"
printf 'toplevel=%s\nreason=%s\n' "$target" "the node agent did not confirm the switch within {GUARD_SECONDS}s" > {SWITCH_FAILED}
nix-env -p {SYSTEM_PROFILE} --set {previous}
{previous}/bin/switch-to-configuration switch
"#,
        files = CANDIDATE_FILES.join(" "),
    )
}

/// Before a live switch: record it and start a timer that switches back to
/// `previous` unless the agent on the new generation confirms it.
fn arm_switch_guard(target: &str, previous: &str, update_dir: &Path) -> Result<(), String> {
    if previous.is_empty() {
        return Err("cannot tell which generation is running; refusing to switch".into());
    }
    fs::write(
        SWITCH_PENDING,
        format!("toplevel={target}\nprevious={previous}\n"),
    )
    .map_err(|e| format!("switch-pending: {e}"))?;
    disarm_switch_guard();
    let tools = resolved(Path::new("/run/current-system/sw"));
    let status = Command::new("systemd-run")
        .args([
            &format!("--unit={GUARD_UNIT}"),
            &format!("--on-active={GUARD_SECONDS}"),
            "--timer-property=AccuracySec=1s",
            &format!("--setenv=PATH={tools}/bin"),
            "--collect",
            &format!("{tools}/bin/sh"),
            "-c",
            &guard_script(previous, update_dir),
        ])
        .status()
        .map_err(|e| format!("systemd-run: {e}"))?;
    if !status.success() {
        let _ = fs::remove_file(SWITCH_PENDING);
        return Err("could not arm the switch guard".into());
    }
    Ok(())
}

fn disarm_switch_guard() {
    let _ = Command::new("systemctl")
        .args([
            "stop",
            &format!("{GUARD_UNIT}.timer"),
            &format!("{GUARD_UNIT}.service"),
        ])
        .status();
    let _ = Command::new("systemctl")
        .args(["reset-failed", &format!("{GUARD_UNIT}.service")])
        .status();
}

/// Switch back to `previous` after a failed switch, from this agent.
fn revert_switch(previous: &str, update_dir: &Path, reason: &str) -> Result<(), String> {
    disarm_switch_guard();
    let target = fs::read_to_string(SWITCH_PENDING).unwrap_or_default();
    let _ = fs::remove_file(SWITCH_PENDING);
    restore_previous(Path::new(HOST_DIR), &update_dir.join("previous"))?;
    let _ = fs::write(
        SWITCH_FAILED,
        format!(
            "toplevel={}\nreason={reason}\n",
            marker_field(&target, "toplevel").unwrap_or("")
        ),
    );
    let status = Command::new("nix-env")
        .args(["-p", SYSTEM_PROFILE, "--set", previous])
        .status()
        .map_err(|e| format!("nix-env --set: {e}"))?;
    if !status.success() {
        return Err("nix-env --set failed".into());
    }
    let status = Command::new(format!("{previous}/bin/switch-to-configuration"))
        .arg("switch")
        .status()
        .map_err(|e| format!("switch-to-configuration: {e}"))?;
    if !status.success() {
        return Err("switch-to-configuration switch failed".into());
    }
    Ok(())
}

/// Called by the agent once it is serving. A pending switch to the
/// generation this agent runs in is confirmed, unless the controller unit
/// failed in it; the guard timer then reverts it.
pub fn confirm_switch() {
    let Ok(body) = fs::read_to_string(SWITCH_PENDING) else {
        return;
    };
    let target = marker_field(&body, "toplevel").unwrap_or("");
    if target.is_empty() || resolved(Path::new("/run/current-system")) != target {
        return;
    }
    let controller_failed = Command::new("systemctl")
        .args(["is-failed", "--quiet", "kcore-controller.service"])
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    if controller_failed {
        return;
    }
    disarm_switch_guard();
    let _ = fs::remove_file(SWITCH_PENDING);
}

pub fn current_generations() -> (String, String) {
    (
        link_target("/run/booted-system"),
        link_target("/run/current-system"),
    )
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
    fn version_pin_is_dropped_from_both_installer_layouts() {
        let iso = "  {\n    services.kcore.version = \"0.6.1\";\n    services.kcore.kctl.package = kcore.packages.x86_64-linux.kctl;\n  }\n";
        let out = strip_version_pin(iso);
        assert!(!out.contains("0.6.1"));
        assert!(out.contains("kctl.package"));
        assert!(out.ends_with('\n'));
        let infect =
            "services.kcore.version = kcore.packages.${system}.kcore-node-agent.version;\nx = 1;";
        assert_eq!(strip_version_pin(infect), "x = 1;");
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
    fn flake_urls_name_a_commit_or_tag() {
        let rev = "0123456789abcdef0123456789abcdef01234567";
        assert_eq!(
            flake_url_at("github:kcorehypervisor/kcore", "v0.7.0"),
            "github:kcorehypervisor/kcore/v0.7.0"
        );
        assert_eq!(
            flake_url_at("github:kcorehypervisor/kcore/", rev),
            format!("github:kcorehypervisor/kcore/{rev}")
        );
        assert_eq!(
            flake_url_at("git+https://mirror.lan/kcore.git", "v0.7.0"),
            "git+https://mirror.lan/kcore.git?ref=refs/tags/v0.7.0"
        );
        assert_eq!(
            flake_url_at("git+https://mirror.lan/kcore.git?shallow=1", rev),
            format!("git+https://mirror.lan/kcore.git?shallow=1&rev={rev}")
        );
    }

    #[test]
    fn release_tags_are_semver_with_a_v() {
        assert!(validate_release_tag("v0.7.0").is_ok());
        assert!(validate_release_tag("v10.20.30").is_ok());
        assert!(validate_release_tag("0.7.0").is_err());
        assert!(validate_release_tag("v0.7").is_err());
        assert!(validate_release_tag("v0.7.0-rc1").is_err());
    }

    #[test]
    fn release_flake_matches_the_iso_template() {
        let root = tempfile::tempdir().unwrap();
        fs::write(root.path().join("flake.lock"), "old").unwrap();
        fs::write(
            root.path().join("configuration.nix"),
            "{ networking.hostName = \"n\"; }",
        )
        .unwrap();
        stage_release_flake(root.path(), "github:kcorehypervisor/kcore/v0.7.0").unwrap();
        let text = fs::read_to_string(root.path().join("flake.nix")).unwrap();
        assert!(text.contains("inputs.kcore.url = \"github:kcorehypervisor/kcore/v0.7.0\";"));
        assert!(text.contains("kcore.lib.mkNode"));
        assert!(!root.path().join("flake.lock").exists());
        assert!(root.path().join("legacy-configuration.nix").exists());
    }

    #[test]
    fn locked_kcore_rev_is_read_from_the_root_input() {
        let lock = r#"{"root":"root","version":7,"nodes":{
            "root":{"inputs":{"kcore":"kcore"}},
            "kcore":{"inputs":{"nixpkgs":"nixpkgs"},"locked":{"rev":"1111111111111111111111111111111111111111"}},
            "nixpkgs":{"locked":{"rev":"3333333333333333333333333333333333333333"}}}}"#;
        assert_eq!(
            locked_input_rev(lock, "kcore").as_deref(),
            Some("1111111111111111111111111111111111111111")
        );
        assert_eq!(locked_input_rev(lock, "missing"), None);
    }

    #[test]
    fn failure_markers_are_read_by_key() {
        let body = "toplevel=/nix/store/abc\nreason=agent did not start\n";
        assert_eq!(marker_field(body, "toplevel"), Some("/nix/store/abc"));
        assert_eq!(marker_field(body, "reason"), Some("agent did not start"));
        assert_eq!(marker_field(body, "previous"), None);
    }

    #[test]
    fn guard_restores_pins_and_switches_to_the_previous_generation() {
        let script = guard_script(
            "/nix/store/old-system",
            Path::new("/var/lib/kcore/updates/u"),
        );
        assert!(script.contains("for f in flake.nix flake.lock legacy-configuration.nix; do"));
        assert!(script.contains("--set /nix/store/old-system"));
        assert!(script.contains("/nix/store/old-system/bin/switch-to-configuration switch"));
        assert!(script.contains(SWITCH_FAILED));
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

    #[test]
    fn release_update_moves_both_pins_on_an_iso_host_flake() {
        let root = tempfile::tempdir().unwrap();
        let flake = root.path().join("flake.nix");
        fs::write(
            &flake,
            "{\n  inputs = {\n    nixpkgs.url = \"path:/nix/store/aaa-source\";\n    kcore.url = \"path:/nix/store/bbb-source\";\n  };\n}\n",
        )
        .unwrap();
        let kcore = "0123456789abcdef0123456789abcdef01234567";
        let nixpkgs = "89abcdef0123456789abcdef0123456789abcdef";
        rewrite_host_inputs(&flake, "kcore", "github:org/kcore", kcore, nixpkgs).unwrap();
        let text = fs::read_to_string(&flake).unwrap();
        assert!(text.contains(&format!("kcore.url = \"github:org/kcore/{kcore}\"")));
        assert!(text.contains(&format!("nixpkgs.url = \"github:NixOS/nixpkgs/{nixpkgs}\"")));
    }

    #[test]
    fn release_update_on_a_follows_host_flake_moves_only_kcore() {
        let root = tempfile::tempdir().unwrap();
        let flake = root.path().join("flake.nix");
        fs::write(
            &flake,
            "{\n  inputs.kcore.url = \"github:org/kcore\";\n  outputs = { kcore, ... }: { x = kcore.inputs.nixpkgs; };\n}\n",
        )
        .unwrap();
        let kcore = "0123456789abcdef0123456789abcdef01234567";
        let nixpkgs = "89abcdef0123456789abcdef0123456789abcdef";
        rewrite_host_inputs(&flake, "kcore", "github:org/kcore", kcore, nixpkgs).unwrap();
        let text = fs::read_to_string(&flake).unwrap();
        assert!(text.contains(&format!("github:org/kcore/{kcore}")));
        assert!(!text.contains(nixpkgs));
        let err =
            rewrite_host_inputs(&flake, "nixpkgs", "github:org/kcore", kcore, nixpkgs).unwrap_err();
        assert!(err.contains("move the kcore rev"));
    }

    #[test]
    fn migration_flake_pins_the_os_through_kcore_without_a_tested_pin() {
        let rev = "0123456789abcdef0123456789abcdef01234567";
        let text = render_migration_flake("github:org/kcore", rev, "");
        assert!(text.contains("nixpkgs.follows = \"kcore/nixpkgs\";"));
        assert!(!text.contains("nixos-unstable"));
        let pinned = render_migration_flake("github:org/kcore", rev, rev);
        assert!(pinned.contains(&format!("github:NixOS/nixpkgs/{rev}")));
    }

    #[test]
    fn staging_copies_the_host_tree_without_offline_sources() {
        let root = tempfile::tempdir().unwrap();
        let live = root.path().join("nixos");
        fs::create_dir_all(live.join("kcore-src/crates")).unwrap();
        fs::create_dir_all(live.join("extra")).unwrap();
        fs::write(live.join("flake.nix"), "flake").unwrap();
        fs::write(live.join("node.nix"), "node").unwrap();
        fs::write(live.join("extra/a.nix"), "a").unwrap();
        fs::write(live.join("kcore-src/flake.nix"), "src").unwrap();
        std::os::unix::fs::symlink("/nonexistent", live.join("result")).unwrap();
        let staging = root.path().join("host");
        copy_host_tree(&live, &staging, &[]).unwrap();
        assert_eq!(
            fs::read_to_string(staging.join("node.nix")).unwrap(),
            "node"
        );
        assert_eq!(
            fs::read_to_string(staging.join("extra/a.nix")).unwrap(),
            "a"
        );
        assert!(!staging.join("kcore-src").exists());
        assert!(!staging.join("result").exists());
    }

    #[test]
    fn restage_keeps_target_pins_and_takes_new_node_config() {
        let root = tempfile::tempdir().unwrap();
        let live = root.path().join("nixos");
        fs::create_dir_all(&live).unwrap();
        fs::write(live.join("flake.nix"), "old pins").unwrap();
        fs::write(live.join("kcore-vms.nix"), "vm a").unwrap();
        let staging = root.path().join("u/host");
        copy_host_tree(&live, &staging, &[]).unwrap();
        fs::write(staging.join("flake.nix"), "new pins").unwrap();
        fs::write(staging.join("flake.lock"), "new lock").unwrap();
        fs::write(live.join("kcore-vms.nix"), "vm a\nvm b").unwrap();
        restage_host_tree(&live, &staging).unwrap();
        assert_eq!(
            fs::read_to_string(staging.join("flake.nix")).unwrap(),
            "new pins"
        );
        assert_eq!(
            fs::read_to_string(staging.join("flake.lock")).unwrap(),
            "new lock"
        );
        assert_eq!(
            fs::read_to_string(staging.join("kcore-vms.nix")).unwrap(),
            "vm a\nvm b"
        );
        assert_eq!(
            fs::read_to_string(live.join("flake.nix")).unwrap(),
            "old pins"
        );
    }

    #[test]
    fn install_and_restore_round_trip_the_host_pins() {
        let root = tempfile::tempdir().unwrap();
        let live = root.path().join("nixos");
        let staging = root.path().join("host");
        let previous = root.path().join("previous");
        fs::create_dir_all(&live).unwrap();
        fs::create_dir_all(&staging).unwrap();
        fs::write(live.join("flake.nix"), "old flake").unwrap();
        fs::write(live.join("flake.lock"), "old lock").unwrap();
        fs::write(staging.join("flake.nix"), "new flake").unwrap();
        fs::write(staging.join("flake.lock"), "new lock").unwrap();
        fs::write(staging.join("legacy-configuration.nix"), "legacy").unwrap();

        install_candidate(&live, &staging, &previous).unwrap();
        // A retried activation must not back up the target pins.
        install_candidate(&live, &staging, &previous).unwrap();
        assert_eq!(
            fs::read_to_string(live.join("flake.nix")).unwrap(),
            "new flake"
        );
        assert_eq!(
            fs::read_to_string(live.join("flake.lock")).unwrap(),
            "new lock"
        );
        assert!(live.join("legacy-configuration.nix").exists());

        assert!(restore_previous(&live, &previous).unwrap());
        assert_eq!(
            fs::read_to_string(live.join("flake.nix")).unwrap(),
            "old flake"
        );
        assert_eq!(
            fs::read_to_string(live.join("flake.lock")).unwrap(),
            "old lock"
        );
        assert!(!live.join("legacy-configuration.nix").exists());
        assert!(!previous.exists());
        assert!(!restore_previous(&live, &previous).unwrap());
    }

    #[test]
    fn vm_unit_content_change_is_detected() {
        let root = tempfile::tempdir().unwrap();
        let store = root.path().join("store");
        fs::create_dir_all(&store).unwrap();
        fs::write(store.join("a.service"), "ExecStart=ch 1").unwrap();
        fs::write(store.join("b.service"), "ExecStart=ch 2").unwrap();
        let mk = |name: &str, unit: &str| {
            let dir = root.path().join(name).join("etc/systemd/system");
            fs::create_dir_all(&dir).unwrap();
            std::os::unix::fs::symlink(store.join(unit), dir.join("kcore-vm-web.service")).unwrap();
            std::os::unix::fs::symlink(store.join("a.service"), dir.join("sshd.service")).unwrap();
            root.path().join(name)
        };
        let old = mk("old", "a.service");
        let same = mk("same", "a.service");
        let changed = mk("changed", "b.service");
        assert_eq!(vm_unit_fingerprints(&old), vm_unit_fingerprints(&same));
        assert_ne!(vm_unit_fingerprints(&old), vm_unit_fingerprints(&changed));
        assert_eq!(vm_unit_fingerprints(&old).len(), 1);
    }

    #[test]
    fn nixpkgs_rev_resolves_through_own_input_and_kcore_follows() {
        let own = r#"{"root":"root","version":7,"nodes":{
            "root":{"inputs":{"kcore":"kcore","nixpkgs":"nixpkgs"}},
            "kcore":{"inputs":{"nixpkgs":"nixpkgs_2"},"locked":{"rev":"1111111111111111111111111111111111111111"}},
            "nixpkgs":{"locked":{"rev":"2222222222222222222222222222222222222222"}},
            "nixpkgs_2":{"locked":{"rev":"3333333333333333333333333333333333333333"}}}}"#;
        assert_eq!(
            locked_nixpkgs_rev(own).as_deref(),
            Some("2222222222222222222222222222222222222222")
        );
        let follows = r#"{"root":"root","version":7,"nodes":{
            "root":{"inputs":{"kcore":"kcore","nixpkgs":["kcore","nixpkgs"]}},
            "kcore":{"inputs":{"nixpkgs":"nixpkgs"},"locked":{"rev":"1111111111111111111111111111111111111111"}},
            "nixpkgs":{"locked":{"rev":"3333333333333333333333333333333333333333"}}}}"#;
        assert_eq!(
            locked_nixpkgs_rev(follows).as_deref(),
            Some("3333333333333333333333333333333333333333")
        );
        let through_kcore = r#"{"root":"root","version":7,"nodes":{
            "root":{"inputs":{"kcore":"kcore"}},
            "kcore":{"inputs":{"nixpkgs":"nixpkgs"},"locked":{"rev":"1111111111111111111111111111111111111111"}},
            "nixpkgs":{"locked":{"rev":"3333333333333333333333333333333333333333"}}}}"#;
        assert_eq!(
            locked_nixpkgs_rev(through_kcore).as_deref(),
            Some("3333333333333333333333333333333333333333")
        );
        let iso = r#"{"root":"root","version":7,"nodes":{
            "root":{"inputs":{"kcore":"kcore","nixpkgs":"nixpkgs"}},
            "kcore":{"locked":{"path":"/nix/store/x"}},
            "nixpkgs":{"locked":{"path":"/nix/store/y"}}}}"#;
        assert_eq!(locked_nixpkgs_rev(iso), None);
    }
}
