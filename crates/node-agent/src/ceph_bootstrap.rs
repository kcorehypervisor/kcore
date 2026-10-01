//! Ceph bootstrap helpers for kcore SAN (manual deploy without cephadm).
//!
//! The bootstrap package is a JSON blob carried in `ApplyCephConfigRequest.keyring`
//! so the controller can distribute the same mon/admin/bootstrap-osd secrets to
//! every member after the first node generates them with `ceph-authtool`.

use serde::{Deserialize, Serialize};
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::Command;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct BootstrapPackage {
    pub version: u32,
    pub fsid: String,
    pub admin_keyring: String,
    pub mon_keyring: String,
    pub bootstrap_osd_keyring: String,
}

#[derive(Debug, Clone)]
pub struct MonMember {
    pub id: String,
    pub addr: String,
}

pub fn parse_mon_map(mon_map: &str) -> Result<Vec<MonMember>, String> {
    let mut out = Vec::new();
    for part in mon_map.split(',') {
        let part = part.trim();
        if part.is_empty() {
            continue;
        }
        let (id, addr) = part
            .split_once('=')
            .ok_or_else(|| format!("invalid mon_map entry (want id=ip): {part}"))?;
        let id = id.trim();
        let addr = addr.trim();
        if id.is_empty() || addr.is_empty() {
            return Err(format!("invalid mon_map entry: {part}"));
        }
        // Strip optional :port from addr for monmaptool --add.
        let addr = addr.split(':').next().unwrap_or(addr).to_string();
        out.push(MonMember {
            id: id.to_string(),
            addr,
        });
    }
    if out.is_empty() {
        return Err("mon_map must list at least one id=ip entry".into());
    }
    Ok(out)
}

pub fn decode_package(bytes: &[u8]) -> Result<BootstrapPackage, String> {
    if bytes.is_empty() {
        return Err("empty bootstrap package".into());
    }
    serde_json::from_slice(bytes).map_err(|e| format!("invalid bootstrap package JSON: {e}"))
}

pub fn encode_package(pkg: &BootstrapPackage) -> Result<Vec<u8>, String> {
    serde_json::to_vec(pkg).map_err(|e| format!("encode bootstrap package: {e}"))
}

fn run(program: &str, args: &[&str]) -> Result<String, String> {
    let out = Command::new(program)
        .args(args)
        .output()
        .map_err(|e| format!("starting {program}: {e}"))?;
    if !out.status.success() {
        return Err(format!(
            "{program} {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    Ok(String::from_utf8_lossy(&out.stdout).to_string())
}

/// Generate mon/admin/bootstrap-osd keyrings with `ceph-authtool` (requires Ceph tools).
pub fn generate_bootstrap_package(fsid: &str) -> Result<BootstrapPackage, String> {
    let tmp = tempfile::tempdir().map_err(|e| format!("tempdir: {e}"))?;
    let mon_kr = tmp.path().join("ceph.mon.keyring");
    let admin_kr = tmp.path().join("ceph.client.admin.keyring");
    let boot_kr = tmp.path().join("ceph.bootstrap-osd.keyring");

    run(
        "ceph-authtool",
        &[
            "--create-keyring",
            mon_kr.to_str().unwrap(),
            "--gen-key",
            "-n",
            "mon.",
            "--cap",
            "mon",
            "allow *",
        ],
    )?;
    run(
        "ceph-authtool",
        &[
            "--create-keyring",
            admin_kr.to_str().unwrap(),
            "--gen-key",
            "-n",
            "client.admin",
            "--cap",
            "mon",
            "allow *",
            "--cap",
            "osd",
            "allow *",
            "--cap",
            "mds",
            "allow *",
            "--cap",
            "mgr",
            "allow *",
        ],
    )?;
    run(
        "ceph-authtool",
        &[
            "--create-keyring",
            boot_kr.to_str().unwrap(),
            "--gen-key",
            "-n",
            "client.bootstrap-osd",
            "--cap",
            "mon",
            "profile bootstrap-osd",
            "--cap",
            "mgr",
            "allow r",
        ],
    )?;
    run(
        "ceph-authtool",
        &[
            mon_kr.to_str().unwrap(),
            "--import-keyring",
            admin_kr.to_str().unwrap(),
        ],
    )?;
    run(
        "ceph-authtool",
        &[
            mon_kr.to_str().unwrap(),
            "--import-keyring",
            boot_kr.to_str().unwrap(),
        ],
    )?;

    Ok(BootstrapPackage {
        version: 1,
        fsid: fsid.to_string(),
        admin_keyring: std::fs::read_to_string(&admin_kr).map_err(|e| e.to_string())?,
        mon_keyring: std::fs::read_to_string(&mon_kr).map_err(|e| e.to_string())?,
        bootstrap_osd_keyring: std::fs::read_to_string(&boot_kr).map_err(|e| e.to_string())?,
    })
}

pub fn write_keyring_files(pkg: &BootstrapPackage) -> Result<(), String> {
    std::fs::create_dir_all("/etc/ceph").map_err(|e| e.to_string())?;
    std::fs::create_dir_all("/var/lib/ceph/bootstrap-osd").map_err(|e| e.to_string())?;
    std::fs::write("/etc/ceph/ceph.client.admin.keyring", &pkg.admin_keyring)
        .map_err(|e| e.to_string())?;
    std::fs::write(
        "/var/lib/ceph/bootstrap-osd/ceph.keyring",
        &pkg.bootstrap_osd_keyring,
    )
    .map_err(|e| e.to_string())?;
    // Combined mon keyring used only for mkfs; keep a copy for operators.
    std::fs::write("/etc/ceph/ceph.mon.keyring", &pkg.mon_keyring).map_err(|e| e.to_string())?;
    for path in [
        "/etc/ceph/ceph.client.admin.keyring",
        "/etc/ceph/ceph.mon.keyring",
        "/var/lib/ceph/bootstrap-osd/ceph.keyring",
    ] {
        let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
    }
    let _ = run(
        "chown",
        &[
            "-R",
            "ceph:ceph",
            "/etc/ceph",
            "/var/lib/ceph/bootstrap-osd",
        ],
    );
    Ok(())
}

pub fn mon_data_dir(daemon_id: &str) -> String {
    format!("/var/lib/ceph/mon/ceph-{daemon_id}")
}

pub fn mon_already_initialized(daemon_id: &str) -> bool {
    Path::new(&mon_data_dir(daemon_id))
        .join("kv_backend")
        .exists()
        || Path::new(&mon_data_dir(daemon_id))
            .join("store.db")
            .exists()
        || Path::new(&mon_data_dir(daemon_id)).join("done").exists()
}

/// A cluster that already has quorum must learn this mon before mkfs.
/// Building a fresh monmap here gives the new daemon a different epoch, so it
/// never joins. Returns true when `monmap_path` holds the cluster's map.
fn adopt_running_monmap(
    daemon_id: &str,
    mons: &[MonMember],
    monmap_path: &Path,
) -> Result<bool, String> {
    let Some(me) = mons.iter().find(|m| m.id == daemon_id) else {
        return Ok(false);
    };
    let addr = format!("{}:6789", me.addr);
    let add_ok = match Command::new("timeout")
        .args(["10", "ceph", "mon", "add", daemon_id, &addr])
        .output()
    {
        Ok(out) => {
            let stderr = String::from_utf8_lossy(&out.stderr);
            out.status.success() || stderr.contains("already")
        }
        Err(_) => false,
    };
    let get_ok = match Command::new("timeout")
        .args(["10", "ceph", "mon", "getmap", "-o"])
        .arg(monmap_path)
        .output()
    {
        Ok(out) => out.status.success() && monmap_path.is_file(),
        Err(_) => false,
    };
    if add_ok && !get_ok {
        return Err(format!(
            "registered mon.{daemon_id} with the cluster but could not fetch the monmap"
        ));
    }
    if !get_ok {
        return Ok(false);
    }
    let printed = Command::new("monmaptool")
        .arg("--print")
        .arg(monmap_path)
        .output()
        .map_err(|e| format!("monmaptool --print: {e}"))?;
    let text = String::from_utf8_lossy(&printed.stdout);
    if !text.contains(&format!("mon.{daemon_id}")) {
        return Err(format!(
            "cluster monmap does not include mon.{daemon_id}; refusing to mkfs a divergent map"
        ));
    }
    Ok(true)
}

pub fn mkfs_mon(pkg: &BootstrapPackage, daemon_id: &str, mons: &[MonMember]) -> Result<(), String> {
    if mon_already_initialized(daemon_id) {
        return Ok(());
    }
    let tmp = tempfile::tempdir().map_err(|e| format!("tempdir: {e}"))?;
    let monmap = tmp.path().join("monmap");
    let mon_kr = tmp.path().join("mon.keyring");
    std::fs::write(&mon_kr, &pkg.mon_keyring).map_err(|e| e.to_string())?;

    let mut args: Vec<String> = vec![
        "--create".into(),
        "--fsid".into(),
        pkg.fsid.clone(),
        "--clobber".into(),
    ];
    for m in mons {
        args.push("--add".into());
        args.push(m.id.clone());
        args.push(format!("{}:6789", m.addr));
    }
    args.push(monmap.display().to_string());
    if !adopt_running_monmap(daemon_id, mons, &monmap)? {
        let args_ref: Vec<&str> = args.iter().map(String::as_str).collect();
        run("monmaptool", &args_ref)?;
    }

    let data = mon_data_dir(daemon_id);
    std::fs::create_dir_all(&data).map_err(|e| e.to_string())?;
    let _ = run("chown", &["-R", "ceph:ceph", &data]);
    run(
        "ceph-mon",
        &[
            "--mkfs",
            "-i",
            daemon_id,
            "--monmap",
            monmap.to_str().unwrap(),
            "--keyring",
            mon_kr.to_str().unwrap(),
        ],
    )?;
    let _ = std::fs::write(format!("{data}/done"), b"");
    let _ = run("chown", &["-R", "ceph:ceph", &data]);
    Ok(())
}

pub fn ensure_mgr_keyring(daemon_id: &str) -> Result<(), String> {
    let dir = format!("/var/lib/ceph/mgr/ceph-{daemon_id}");
    let keyring = format!("{dir}/keyring");
    if Path::new(&keyring).exists() {
        return Ok(());
    }
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    // Wait briefly for mon quorum after mkfs+start.
    for _ in 0..30 {
        if run(
            "ceph",
            &[
                "auth",
                "get-or-create",
                "-o",
                &keyring,
                &format!("mgr.{daemon_id}"),
                "mon",
                "allow profile mgr",
                "osd",
                "allow *",
                "mds",
                "allow *",
            ],
        )
        .is_ok()
        {
            let _ = run("chown", &["-R", "ceph:ceph", &dir]);
            return Ok(());
        }
        std::thread::sleep(std::time::Duration::from_secs(2));
    }
    Err(format!(
        "timed out creating mgr.{daemon_id} keyring (is ceph-mon up?)"
    ))
}

pub const BOOTSTRAP_STATE_PATH: &str = "/var/lib/kcore/ceph-bootstrap.json";

/// One OSD as reported by `ceph-volume lvm list --format json`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ListedOsd {
    pub id: String,
    pub devices: Vec<String>,
}

/// Reuse a package the controller already distributed, else the copy this node
/// wrote before a previous attempt died, else generate a new one.
///
/// Generating on every empty request is what split a cluster: the first apply
/// created keys, `nixos-rebuild switch` killed the agent before it could return
/// them, and the retry minted a second set over a mon that had already been
/// mkfs'd with the first.
pub fn resolve_bootstrap_package(fsid: &str, provided: &[u8]) -> Result<BootstrapPackage, String> {
    resolve_bootstrap_package_at(Path::new(BOOTSTRAP_STATE_PATH), fsid, provided)
}

pub fn resolve_bootstrap_package_at(
    path: &Path,
    fsid: &str,
    provided: &[u8],
) -> Result<BootstrapPackage, String> {
    if !provided.is_empty() {
        let pkg = decode_package(provided)?;
        if pkg.fsid != fsid {
            return Err(format!(
                "bootstrap package fsid {} does not match request fsid {fsid}",
                pkg.fsid
            ));
        }
        store_package_at(path, &pkg)?;
        return Ok(pkg);
    }
    if let Some(pkg) = load_package_at(path)? {
        if pkg.fsid != fsid {
            return Err(format!(
                "stored bootstrap fsid {} does not match request fsid {fsid}",
                pkg.fsid
            ));
        }
        return Ok(pkg);
    }
    let pkg = generate_bootstrap_package(fsid)?;
    store_package_at(path, &pkg)?;
    Ok(pkg)
}

fn load_package_at(path: &Path) -> Result<Option<BootstrapPackage>, String> {
    match std::fs::read(path) {
        Ok(bytes) => Ok(Some(decode_package(&bytes)?)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(format!("reading {}: {e}", path.display())),
    }
}

fn store_package_at(path: &Path, pkg: &BootstrapPackage) -> Result<(), String> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| format!("creating {}: {e}", dir.display()))?;
    }
    let bytes = encode_package(pkg)?;
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, &bytes).map_err(|e| format!("writing {}: {e}", tmp.display()))?;
    std::fs::rename(&tmp, path).map_err(|e| format!("renaming bootstrap package: {e}"))?;
    Ok(())
}

pub fn parse_ceph_volume_lvm_list(json: &str) -> Result<Vec<ListedOsd>, String> {
    let value: serde_json::Value =
        serde_json::from_str(json).map_err(|e| format!("ceph-volume list JSON: {e}"))?;
    let Some(obj) = value.as_object() else {
        return Ok(Vec::new());
    };
    let mut out = Vec::new();
    for (key, val) in obj {
        let entries: Vec<&serde_json::Value> = match val {
            serde_json::Value::Array(items) => items.iter().collect(),
            other => vec![other],
        };
        for entry in entries {
            let id = entry
                .get("osd_id")
                .and_then(|v| {
                    v.as_i64()
                        .map(|n| n.to_string())
                        .or_else(|| v.as_str().map(str::to_string))
                })
                .or_else(|| {
                    entry
                        .pointer("/tags/ceph.osd_id")
                        .and_then(|v| v.as_str())
                        .map(str::to_string)
                })
                .unwrap_or_else(|| key.clone());
            let devices = entry
                .get("devices")
                .and_then(|v| v.as_array())
                .map(|arr| {
                    arr.iter()
                        .filter_map(|v| v.as_str().map(str::to_string))
                        .filter(|s| !s.is_empty())
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default();
            if devices.is_empty() {
                continue;
            }
            out.push(ListedOsd { id, devices });
        }
    }
    Ok(out)
}

/// True when `device` is the same block device as `wanted`.
///
/// `canonicalize` follows `/dev/disk/by-id` links so a later reconcile that
/// names the kernel path does not retire an OSD it just created under the
/// by-id path. When either path cannot be resolved, only an exact string
/// match counts — a failed stat must not look like "different disk".
pub fn same_block_device(device: &str, wanted: &str) -> bool {
    if device == wanted {
        return true;
    }
    match (std::fs::canonicalize(device), std::fs::canonicalize(wanted)) {
        (Ok(a), Ok(b)) => a == b,
        _ => false,
    }
}

pub fn rbd_size_mib(size_bytes: i64) -> Result<u64, String> {
    if size_bytes <= 0 {
        return Err("size_bytes must be positive".into());
    }
    let mib = (size_bytes as u64).div_ceil(1024 * 1024);
    Ok(mib.max(1))
}

pub fn parse_osd_counters(json: &serde_json::Value) -> (i32, i32) {
    let up = json
        .pointer("/osdmap/num_up_osds")
        .or_else(|| json.pointer("/osdmap/osdmap/num_up_osds"))
        .and_then(|v| v.as_i64())
        .unwrap_or(0) as i32;
    let inn = json
        .pointer("/osdmap/num_in_osds")
        .or_else(|| json.pointer("/osdmap/osdmap/num_in_osds"))
        .and_then(|v| v.as_i64())
        .unwrap_or(0) as i32;
    (up, inn)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_mon_map_strips_ports() {
        let m = parse_mon_map("dell-1=10.10.0.11:6789,dell-2=10.10.0.12").unwrap();
        assert_eq!(m.len(), 2);
        assert_eq!(m[0].id, "dell-1");
        assert_eq!(m[0].addr, "10.10.0.11");
        assert_eq!(m[1].addr, "10.10.0.12");
    }

    #[test]
    fn parse_mon_map_rejects_empty() {
        assert!(parse_mon_map("").is_err());
        assert!(parse_mon_map("bad").is_err());
    }

    #[test]
    fn bootstrap_package_round_trip() {
        let pkg = BootstrapPackage {
            version: 1,
            fsid: "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee".into(),
            admin_keyring: "[client.admin]\n\tkey = AAA=\n".into(),
            mon_keyring: "[mon.]\n\tkey = BBB=\n".into(),
            bootstrap_osd_keyring: "[client.bootstrap-osd]\n\tkey = CCC=\n".into(),
        };
        let bytes = encode_package(&pkg).unwrap();
        assert_eq!(decode_package(&bytes).unwrap(), pkg);
    }

    #[test]
    fn rbd_size_mib_rounds_up_from_bytes() {
        assert_eq!(rbd_size_mib(1).unwrap(), 1);
        assert_eq!(rbd_size_mib(1024 * 1024).unwrap(), 1);
        assert_eq!(rbd_size_mib(1024 * 1024 + 1).unwrap(), 2);
        assert_eq!(rbd_size_mib(40 * 1024 * 1024 * 1024).unwrap(), 40960);
        assert!(rbd_size_mib(0).is_err());
    }

    #[test]
    fn resolve_bootstrap_package_reuses_stored_keys_for_the_same_fsid() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ceph-bootstrap.json");
        let pkg = BootstrapPackage {
            version: 1,
            fsid: "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee".into(),
            admin_keyring: "admin".into(),
            mon_keyring: "mon".into(),
            bootstrap_osd_keyring: "boot".into(),
        };
        let bytes = encode_package(&pkg).unwrap();
        let first = resolve_bootstrap_package_at(&path, &pkg.fsid, &bytes).unwrap();
        assert_eq!(first, pkg);
        let again = resolve_bootstrap_package_at(&path, &pkg.fsid, &[]).unwrap();
        assert_eq!(again, pkg);
        let err = resolve_bootstrap_package_at(&path, "other-fsid", &[]).unwrap_err();
        assert!(err.contains("does not match"));
    }

    #[test]
    fn parse_ceph_volume_lvm_list_reads_osd_id_and_devices() {
        let json = r#"{"0":[{"osd_id":0,"devices":["/dev/nvme0n1"]}]}"#;
        let listed = parse_ceph_volume_lvm_list(json).unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].id, "0");
        assert_eq!(listed[0].devices, vec!["/dev/nvme0n1".to_string()]);
        assert!(parse_ceph_volume_lvm_list("{}").unwrap().is_empty());
    }

    #[test]
    fn same_block_device_does_not_treat_a_prefix_as_the_same_disk() {
        assert!(same_block_device("/dev/sda", "/dev/sda"));
        assert!(!same_block_device("/dev/sda", "/dev/sda1"));
    }

    fn parse_osd_counters_supports_both_json_shapes() {
        let modern = serde_json::json!({"osdmap":{"num_up_osds":3,"num_in_osds":3}});
        assert_eq!(parse_osd_counters(&modern), (3, 3));
        let nested = serde_json::json!({"osdmap":{"osdmap":{"num_up_osds":2,"num_in_osds":1}}});
        assert_eq!(parse_osd_counters(&nested), (2, 1));
    }
}
