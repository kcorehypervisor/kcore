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

/// Default wall-clock limit for `ceph` / `rados` CLI calls that can hang when
/// quorum is missing.
pub const CEPH_CLI_TIMEOUT_SECS: &str = "30";

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

/// Run a program under `timeout(1)` so missing quorum cannot block forever.
fn run_timeout(secs: &str, program: &str, args: &[&str]) -> Result<String, String> {
    let mut full = Vec::with_capacity(args.len() + 2);
    full.push(secs);
    full.push(program);
    full.extend_from_slice(args);
    let out = Command::new("timeout")
        .args(&full)
        .output()
        .map_err(|e| format!("starting timeout {program}: {e}"))?;
    if !out.status.success() {
        let code = out.status.code();
        let stderr = String::from_utf8_lossy(&out.stderr).trim().to_string();
        if code == Some(124) {
            return Err(format!(
                "{program} {} timed out after {secs}s",
                args.join(" ")
            ));
        }
        return Err(format!("{program} {} failed: {stderr}", args.join(" ")));
    }
    Ok(String::from_utf8_lossy(&out.stdout).to_string())
}

fn run_ceph(args: &[&str]) -> Result<String, String> {
    run_timeout(CEPH_CLI_TIMEOUT_SECS, "ceph", args)
}

/// Members to put in a *fresh* monmap for `daemon_id`.
///
/// The first mon must mkfs with only itself so it can form a single-member
/// quorum; later members adopt the running map via `ceph mon add`.
pub fn fresh_monmap_member<'a>(
    daemon_id: &str,
    mons: &'a [MonMember],
) -> Result<&'a MonMember, String> {
    mons.iter()
        .find(|m| m.id == daemon_id)
        .ok_or_else(|| format!("daemon {daemon_id} missing from mon_map"))
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

/// Only the first mon in the spec may create a brand-new single-member
/// monmap, and only when the controller says this cluster has not been
/// founded yet. Any other member, or a first mon whose keys already exist,
/// must fail instead of founding a second cluster with the same fsid.
pub fn may_found_cluster(daemon_id: &str, mons: &[MonMember], allow_found: bool) -> bool {
    allow_found && mons.first().is_some_and(|m| m.id == daemon_id)
}

/// Both messenger versions, so a mon never joins as v1-only. For `monmaptool
/// --addv` only: `ceph mon add` rejects this form (its argument parser crashes
/// on it in Ceph 20) and takes a bare IP, which also means v2 plus v1.
pub fn mon_addrvec(ip: &str) -> String {
    format!("[v2:{ip}:3300,v1:{ip}:6789]")
}

fn saved_monmap_path(daemon_id: &str) -> String {
    format!("/var/lib/kcore/ceph-monmap-{daemon_id}.bin")
}

/// A cluster that already has quorum must learn this mon before mkfs.
/// Building a fresh monmap here gives the new daemon a different epoch, so it
/// never joins. Returns true when `monmap_path` holds the cluster's map.
///
/// The map is fetched *before* `ceph mon add`: going from one mon to two makes
/// quorum need both, so once the new mon is registered but not yet running no
/// `ceph` command can answer. The fetched map is kept on disk so a retry after
/// that point can still mkfs from it.
fn adopt_running_monmap(
    daemon_id: &str,
    mons: &[MonMember],
    monmap_path: &Path,
    allow_found: bool,
) -> Result<bool, String> {
    let Some(me) = mons.iter().find(|m| m.id == daemon_id) else {
        return Ok(false);
    };
    let addr = mon_addrvec(&me.addr);
    let saved = saved_monmap_path(daemon_id);

    let fetched = match Command::new("timeout")
        .args(["15", "ceph", "mon", "getmap", "-o"])
        .arg(monmap_path)
        .output()
    {
        Ok(out) => out.status.success() && monmap_path.is_file(),
        Err(_) => false,
    };
    if fetched {
        let _ = std::fs::create_dir_all("/var/lib/kcore");
        std::fs::copy(monmap_path, &saved).map_err(|e| format!("save monmap: {e}"))?;
    } else if Path::new(&saved).is_file() {
        std::fs::copy(&saved, monmap_path).map_err(|e| format!("restore saved monmap: {e}"))?;
    } else if may_found_cluster(daemon_id, mons, allow_found) {
        return Ok(false);
    } else {
        return Err(format!(
            "no running Ceph cluster answered `ceph mon getmap` and mon.{daemon_id} is not the \
             first mon; refusing to create a second cluster with the same fsid"
        ));
    }

    let printed = run(
        "monmaptool",
        &["--print", &monmap_path.display().to_string()],
    )?;
    if !printed.contains(&format!("mon.{daemon_id}")) {
        run(
            "monmaptool",
            &[
                "--addv",
                daemon_id,
                &addr,
                &monmap_path.display().to_string(),
            ],
        )?;
    }

    let out = Command::new("timeout")
        .args(["20", "ceph", "mon", "add", daemon_id, &me.addr])
        .output()
        .map_err(|e| format!("ceph mon add: {e}"))?;
    let stderr = String::from_utf8_lossy(&out.stderr);
    let stdout = String::from_utf8_lossy(&out.stdout);
    let already = stderr.contains("already") || stdout.contains("already");
    // A hung `mon add` on retry means it was registered earlier and quorum is
    // waiting for this mon; the saved map is enough to continue.
    let timed_out = out.status.code() == Some(124);
    if !out.status.success() && !already && !timed_out {
        return Err(format!(
            "ceph mon add {daemon_id} {} failed: {stderr}",
            me.addr
        ));
    }
    Ok(true)
}

pub fn mkfs_mon(
    pkg: &BootstrapPackage,
    daemon_id: &str,
    mons: &[MonMember],
    allow_found: bool,
) -> Result<(), String> {
    if mon_already_initialized(daemon_id) {
        return Ok(());
    }
    let tmp = tempfile::tempdir().map_err(|e| format!("tempdir: {e}"))?;
    let monmap = tmp.path().join("monmap");
    let mon_kr = tmp.path().join("mon.keyring");
    std::fs::write(&mon_kr, &pkg.mon_keyring).map_err(|e| e.to_string())?;

    if !adopt_running_monmap(daemon_id, mons, &monmap, allow_found)? {
        // First mon: single-member monmap so this daemon can form quorum alone.
        // Listing every future member here left them as 0.0.0.0 placeholders and
        // the mon stayed in "probing" forever — blocking ensure_mgr_keyring and
        // the reconciler loop before peers were ever applied.
        let me = fresh_monmap_member(daemon_id, mons)?;
        run(
            "monmaptool",
            &[
                "--create",
                "--fsid",
                &pkg.fsid,
                "--clobber",
                "--addv",
                &me.id,
                &mon_addrvec(&me.addr),
                &monmap.display().to_string(),
            ],
        )?;
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
    // Wait briefly for mon quorum after mkfs+start. Each ceph call is bounded.
    for _ in 0..30 {
        if run_ceph(&[
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
        ])
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

/// `ceph osd ls-tree <host> -f json`: a JSON array of OSD ids.
pub fn parse_osd_ls_tree(stdout: &str) -> Result<Vec<String>, String> {
    let ids: Vec<serde_json::Value> =
        serde_json::from_str(stdout.trim()).map_err(|e| format!("osd ls-tree json: {e}"))?;
    ids.iter()
        .map(|v| match v {
            serde_json::Value::Number(n) => Ok(n.to_string()),
            serde_json::Value::String(s) => Ok(s.clone()),
            other => Err(format!("unexpected osd id {other}")),
        })
        .collect()
}

/// Placement-group totals from `ceph -s --format json`: `(total, not active+clean)`.
///
/// Anything other than exactly `active+clean` (degraded, undersized,
/// remapped, backfilling, peering...) still depends on an OSD somewhere, so
/// a host whose OSDs were marked out is only free once this reaches zero.
pub fn parse_pg_clean(json: &serde_json::Value) -> (i32, i32) {
    let total = json
        .pointer("/pgmap/num_pgs")
        .and_then(|v| v.as_i64())
        .unwrap_or(0) as i32;
    let clean = json
        .pointer("/pgmap/pgs_by_state")
        .and_then(|v| v.as_array())
        .map(|states| {
            states
                .iter()
                .filter(|s| s.get("state_name").and_then(|n| n.as_str()) == Some("active+clean"))
                .filter_map(|s| s.get("count").and_then(|c| c.as_i64()))
                .sum::<i64>()
        })
        .unwrap_or(0) as i32;
    (total, (total - clean).max(0))
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedCephDf {
    pub total_bytes: i64,
    pub used_bytes: i64,
    pub avail_bytes: i64,
    pub by_class: Vec<ParsedDeviceClass>,
    pub pools: Vec<ParsedPoolUsage>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedDeviceClass {
    pub device_class: String,
    pub total_bytes: i64,
    pub used_bytes: i64,
    pub avail_bytes: i64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedPoolUsage {
    pub name: String,
    pub stored_bytes: i64,
    pub objects: i64,
    pub percent_used_bp: i32,
    pub max_avail_bytes: i64,
}

/// Typed reading of `ceph df --format json` (cluster totals, device classes, pools).
pub fn parse_ceph_df(json: &serde_json::Value) -> ParsedCephDf {
    let total_bytes = json
        .pointer("/stats/total_bytes")
        .and_then(|v| v.as_i64())
        .unwrap_or(0);
    let used_bytes = json
        .pointer("/stats/total_used_bytes")
        .and_then(|v| v.as_i64())
        .unwrap_or(0);
    let avail_bytes = json
        .pointer("/stats/total_avail_bytes")
        .and_then(|v| v.as_i64())
        .unwrap_or(0);

    let mut by_class = Vec::new();
    if let Some(classes) = json.get("stats_by_class").and_then(|v| v.as_object()) {
        for (name, stats) in classes {
            by_class.push(ParsedDeviceClass {
                device_class: name.clone(),
                total_bytes: stats
                    .get("total_bytes")
                    .and_then(|v| v.as_i64())
                    .unwrap_or(0),
                used_bytes: stats
                    .get("total_used_bytes")
                    .and_then(|v| v.as_i64())
                    .unwrap_or(0),
                avail_bytes: stats
                    .get("total_avail_bytes")
                    .and_then(|v| v.as_i64())
                    .unwrap_or(0),
            });
        }
        by_class.sort_by(|a, b| a.device_class.cmp(&b.device_class));
    }

    let mut pools = Vec::new();
    if let Some(items) = json.get("pools").and_then(|v| v.as_array()) {
        for pool in items {
            let name = pool
                .get("name")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            if name.is_empty() {
                continue;
            }
            let stats = pool
                .get("stats")
                .cloned()
                .unwrap_or(serde_json::Value::Null);
            let percent = stats
                .get("percent_used")
                .and_then(|v| v.as_f64())
                .unwrap_or(0.0);
            pools.push(ParsedPoolUsage {
                name,
                stored_bytes: stats.get("stored").and_then(|v| v.as_i64()).unwrap_or(0),
                objects: stats.get("objects").and_then(|v| v.as_i64()).unwrap_or(0),
                percent_used_bp: (percent * 100.0).round() as i32,
                max_avail_bytes: stats.get("max_avail").and_then(|v| v.as_i64()).unwrap_or(0),
            });
        }
        pools.sort_by(|a, b| a.name.cmp(&b.name));
    }

    ParsedCephDf {
        total_bytes,
        used_bytes,
        avail_bytes,
        by_class,
        pools,
    }
}

/// `up` when any OSD under `host` is up, `down` when every one is down,
/// `absent` when the crush host or its OSDs are missing.
/// `degraded` only when Ceph answered and every OSD on `host` is down.
/// A missing `ceph` binary or a failed query stays `ok` so a probe blip
/// does not cordon the host.
pub fn local_osd_health(host: &str) -> &'static str {
    let Ok(out) = Command::new("timeout")
        .args(["15", "ceph", "osd", "tree", "--format", "json"])
        .output()
    else {
        return "ok";
    };
    if !out.status.success() {
        return "ok";
    }
    let text = String::from_utf8_lossy(&out.stdout);
    match host_osd_state(&text, host) {
        Ok("down") => "degraded",
        _ => "ok",
    }
}

pub fn host_osd_state(tree_json: &str, host: &str) -> Result<&'static str, String> {
    let value: serde_json::Value =
        serde_json::from_str(tree_json).map_err(|e| format!("osd tree: {e}"))?;
    let nodes = value
        .get("nodes")
        .and_then(|n| n.as_array())
        .ok_or_else(|| "osd tree has no nodes".to_string())?;
    let Some(crush_host) = nodes.iter().find(|node| {
        node.get("type").and_then(|t| t.as_str()) == Some("host")
            && node.get("name").and_then(|n| n.as_str()) == Some(host)
    }) else {
        return Ok("absent");
    };
    let children = crush_host
        .get("children")
        .and_then(|c| c.as_array())
        .cloned()
        .unwrap_or_default();
    let mut saw = false;
    let mut any_up = false;
    for child in children {
        let Some(id) = child.as_i64() else {
            continue;
        };
        let Some(osd) = nodes.iter().find(|node| {
            node.get("type").and_then(|t| t.as_str()) == Some("osd")
                && node.get("id").and_then(|n| n.as_i64()) == Some(id)
        }) else {
            continue;
        };
        saw = true;
        if osd.get("status").and_then(|s| s.as_str()) == Some("up") {
            any_up = true;
        }
    }
    if !saw {
        return Ok("absent");
    }
    Ok(if any_up { "up" } else { "down" })
}

/// True when a watcher address belongs to one of `hosts` (IP, ignoring port,
/// nonce, and the `v1:` / `v2:` messenger prefix).
pub fn watcher_matches_host(address: &str, hosts: &[String]) -> bool {
    let addr = address.trim();
    let addr = addr
        .strip_prefix("v1:")
        .or_else(|| addr.strip_prefix("v2:"))
        .unwrap_or(addr);
    let host = if let Some(rest) = addr.strip_prefix('[') {
        rest.split(']').next().unwrap_or("")
    } else {
        let without_nonce = addr.split('/').next().unwrap_or(addr);
        without_nonce
            .rsplit_once(':')
            .map(|(ip, _)| ip)
            .unwrap_or(without_nonce)
    };
    !host.is_empty() && hosts.iter().any(|candidate| candidate == host)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn osd_ls_tree_accepts_numbers_and_strings() {
        assert_eq!(parse_osd_ls_tree("[0,2]\n").unwrap(), vec!["0", "2"]);
        assert_eq!(parse_osd_ls_tree("[]").unwrap(), Vec::<String>::new());
        assert!(parse_osd_ls_tree("Error ENOENT").is_err());
        assert!(parse_osd_ls_tree("[{}]").is_err());
    }

    #[test]
    fn pg_clean_counts_only_active_clean() {
        let json: serde_json::Value = serde_json::json!({
            "pgmap": {
                "num_pgs": 129,
                "pgs_by_state": [
                    {"state_name": "active+clean", "count": 100},
                    {"state_name": "active+undersized+degraded", "count": 20},
                    {"state_name": "active+remapped+backfilling", "count": 9}
                ]
            }
        });
        assert_eq!(parse_pg_clean(&json), (129, 29));
        let json: serde_json::Value = serde_json::json!({
            "pgmap": {"num_pgs": 33, "pgs_by_state": [{"state_name": "active+clean", "count": 33}]}
        });
        assert_eq!(parse_pg_clean(&json), (33, 0));
        // No PGs at all (fresh cluster with no pools) counts as clean.
        assert_eq!(parse_pg_clean(&serde_json::json!({})), (0, 0));
    }

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
    fn mon_addrvec_carries_both_messenger_versions() {
        assert_eq!(
            mon_addrvec("10.0.0.1"),
            "[v2:10.0.0.1:3300,v1:10.0.0.1:6789]"
        );
    }

    #[test]
    fn osd_tree_reports_the_named_host() {
        let json = r#"{"nodes":[
            {"id":-3,"name":"node-103","type":"host","children":[0]},
            {"id":0,"name":"osd.0","type":"osd","status":"up"},
            {"id":-4,"name":"node-118","type":"host","children":[1]},
            {"id":1,"name":"osd.1","type":"osd","status":"down"}
        ]}"#;
        assert_eq!(host_osd_state(json, "node-103").unwrap(), "up");
        assert_eq!(host_osd_state(json, "node-118").unwrap(), "down");
        assert_eq!(host_osd_state(json, "node-119").unwrap(), "absent");
        assert!(watcher_matches_host(
            "192.168.40.103:0/9",
            &["192.168.40.103".into()]
        ));
        assert!(watcher_matches_host(
            "v1:192.168.40.103:0/9",
            &["192.168.40.103".into()]
        ));
        assert!(watcher_matches_host(
            "[2001:db8::103]:0/9",
            &["2001:db8::103".into()]
        ));
        assert!(!watcher_matches_host(
            "192.168.40.145:0/9",
            &["192.168.40.103".into()]
        ));
    }

    #[test]
    fn only_the_first_mon_may_found_a_cluster() {
        let mons = parse_mon_map("dell-1=10.10.0.11,dell-2=10.10.0.12,dell-3=10.10.0.13").unwrap();
        assert!(may_found_cluster("dell-1", &mons, true));
        assert!(!may_found_cluster("dell-2", &mons, true));
        assert!(!may_found_cluster("dell-3", &mons, true));
        assert!(!may_found_cluster("dell-1", &[], true));
        assert!(!may_found_cluster("dell-1", &mons, false));
    }

    #[test]
    fn fresh_monmap_member_is_only_the_local_daemon() {
        let mons = parse_mon_map("dell-1=10.10.0.11,dell-2=10.10.0.12,dell-3=10.10.0.13").unwrap();
        let me = fresh_monmap_member("dell-2", &mons).unwrap();
        assert_eq!(me.id, "dell-2");
        assert_eq!(me.addr, "10.10.0.12");
        // First-mon path must never expand to the full membership list.
        assert_eq!(fresh_monmap_member("dell-1", &mons).unwrap().id, "dell-1");
        assert!(fresh_monmap_member("missing", &mons).is_err());
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

    #[test]
    fn parse_ceph_df_reads_classes_and_pools() {
        let json = serde_json::json!({
            "stats": {
                "total_bytes": 1000,
                "total_used_bytes": 400,
                "total_avail_bytes": 600
            },
            "stats_by_class": {
                "ssd": {
                    "total_bytes": 300,
                    "total_used_bytes": 100,
                    "total_avail_bytes": 200
                },
                "hdd": {
                    "total_bytes": 700,
                    "total_used_bytes": 300,
                    "total_avail_bytes": 400
                }
            },
            "pools": [{
                "name": "kcore-vms",
                "stats": {
                    "stored": 50,
                    "objects": 3,
                    "percent_used": 12.5,
                    "max_avail": 200
                }
            }]
        });
        let parsed = parse_ceph_df(&json);
        assert_eq!(parsed.total_bytes, 1000);
        assert_eq!(parsed.used_bytes, 400);
        assert_eq!(parsed.avail_bytes, 600);
        assert_eq!(parsed.by_class.len(), 2);
        assert_eq!(parsed.by_class[0].device_class, "hdd");
        assert_eq!(parsed.by_class[1].device_class, "ssd");
        assert_eq!(parsed.by_class[1].total_bytes, 300);
        assert_eq!(parsed.pools.len(), 1);
        assert_eq!(parsed.pools[0].percent_used_bp, 1250);
        assert_eq!(parsed.pools[0].max_avail_bytes, 200);
    }
}
