//! Block-device mirror used to live-migrate LVM and ZFS guests.
//!
//! Cloud Hypervisor copies memory only. Ceph gives both nodes the same RBD
//! device; a local LV or zvol does not, so this module replicates the block
//! device itself. The guest keeps writing to a stable device-mapper name
//! (`/dev/mapper/kcore-vm-<name>`). For a live move that mapping is reloaded
//! from `linear` to `mirror`, with the second leg an NBD client pointed at a
//! same-sized LV/zvol on the destination. `dm-mirror` completes a write only
//! after both legs have it, and the VMM opens the disk with `O_DIRECT`, so a
//! fully synced mirror has every acknowledged guest write on the destination
//! before memory migration starts.
//!
//! Command lines and status parsing are pure. The functions that run them are
//! separate and only used on a node.

use std::fs;
use std::io::Write;
use std::net::IpAddr;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::lvm_ops::{self, LvmOpError};

/// Ports for the NBD export. Sits directly above the Cloud Hypervisor
/// migration range (18000–18127) and below the kernel ephemeral range.
pub const NBD_PORT_BASE: u16 = 18128;
pub const NBD_PORT_COUNT: u16 = 128;

/// `dm-mirror` region size in 512-byte sectors (16 MiB).
///
/// Recovery copies one region and waits for the NBD write before the next.
/// A 512 KiB region stayed near 11 MiB/s on the lab's 10 GbE link, so a 20 GiB
/// disk took about half an hour. The kernel accepts a power-of-two region no
/// larger than the device; 16 MiB divides a 20 GiB volume evenly. Shorter
/// disks use the largest power of two that still fits.
pub const MIRROR_REGION_SECTORS: u64 = 32768;

const MIN_MIRROR_REGION_SECTORS: u64 = 8;

pub const SEED_MARKER_DIR: &str = "/var/lib/kcore/volume-seeded";

const SESSION_SUFFIX: &str = ".volume-mirror.json";
const NBD_PID_SUFFIX: &str = ".nbd.pid";

/// What `dmsetup status` says about one mapping.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DmStatus {
    /// `length` is 512-byte sectors.
    Linear {
        sectors: u64,
    },
    Mirror(MirrorProgress),
}

/// Sync progress of a `dm-mirror` mapping.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MirrorProgress {
    pub sectors: u64,
    pub bytes_synced: u64,
    pub bytes_total: u64,
    /// Every region is copied and every leg reports healthy (`A`).
    pub in_sync: bool,
    /// A leg reported a read, write, flush, or sync error.
    pub degraded: bool,
}

/// Disk facts the controller needs before it chooses live or cold.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GuestDiskFacts {
    pub running: bool,
    pub guest_uses_mapper: bool,
    pub memory_shared: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MirrorSession {
    pub role: String,
    pub vm_name: String,
    pub backend: String,
    pub vg_name: String,
    pub pool_name: String,
    pub export_name: String,
    pub port: u16,
    pub bind_addr: String,
    pub nbd_device: String,
    pub backing_path: String,
    pub mapper_path: String,
    pub nbd_pid: u32,
    pub size_bytes: u64,
}

impl MirrorSession {
    pub fn is_target(&self) -> bool {
        self.role == "target"
    }

    pub fn is_source(&self) -> bool {
        self.role == "source"
    }
}

/// Rejects anything that would be dangerous interpolated into a device name.
pub fn validate_vm_name(name: &str) -> Result<String, String> {
    let name = name.trim();
    if name.is_empty() || name.len() > 80 {
        return Err("vm_name must be 1–80 characters".into());
    }
    if !name
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    {
        return Err("vm_name must be letters, digits, '_' or '-'".into());
    }
    Ok(name.to_string())
}

pub fn mapper_name(vm: &str) -> Result<String, String> {
    Ok(format!("kcore-vm-{}", validate_vm_name(vm)?))
}

pub fn mapper_path(vm: &str) -> Result<String, String> {
    Ok(format!("/dev/mapper/{}", mapper_name(vm)?))
}

pub fn lv_name(vm: &str) -> Result<String, String> {
    // Matches `lvName` in modules/ch-vm/vm-service.nix.
    Ok(format!("kcore-{}", validate_vm_name(vm)?))
}

pub fn lvm_backing(vg: &str, vm: &str) -> Result<String, String> {
    let vg = validate_storage_name(vg, "volume group")?;
    Ok(format!("/dev/{vg}/{}", lv_name(vm)?))
}

pub fn zfs_dataset(pool: &str, vm: &str) -> Result<String, String> {
    let pool = validate_storage_name(pool, "zfs pool")?;
    Ok(format!("{pool}/{}", lv_name(vm)?))
}

pub fn zfs_backing(pool: &str, vm: &str) -> Result<String, String> {
    Ok(format!("/dev/zvol/{}", zfs_dataset(pool, vm)?))
}

pub fn backing_path(backend: &str, vg: &str, pool: &str, vm: &str) -> Result<String, String> {
    match backend.trim() {
        "lvm" => lvm_backing(vg, vm),
        "zfs" => zfs_backing(pool, vm),
        other => Err(format!("volume mirror supports lvm or zfs, not {other}")),
    }
}

pub fn seed_marker_path(vm: &str) -> Result<PathBuf, String> {
    Ok(PathBuf::from(SEED_MARKER_DIR).join(validate_vm_name(vm)?))
}

pub fn session_path(socket_dir: &Path, vm: &str) -> Result<PathBuf, String> {
    Ok(socket_dir.join(format!("{}{SESSION_SUFFIX}", validate_vm_name(vm)?)))
}

pub fn nbd_pid_path(socket_dir: &Path, vm: &str) -> Result<PathBuf, String> {
    Ok(socket_dir.join(format!("{}{NBD_PID_SUFFIX}", validate_vm_name(vm)?)))
}

fn validate_storage_name(name: &str, what: &str) -> Result<String, String> {
    lvm_ops::validate_component(name.trim(), what).map_err(|e: LvmOpError| e.to_string())
}

pub fn validate_export_name(name: &str) -> Result<String, String> {
    let name = name.trim();
    if name.is_empty() || name.len() > 80 {
        return Err("export_name must be 1–80 characters".into());
    }
    if !name
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    {
        return Err("export_name must be letters, digits, '_' or '-'".into());
    }
    Ok(name.to_string())
}

pub fn export_name(vm: &str, nonce_hex: &str) -> Result<String, String> {
    let vm = validate_vm_name(vm)?;
    let nonce = nonce_hex.trim();
    if nonce.len() < 8 || nonce.len() > 32 || !nonce.chars().all(|c| c.is_ascii_hexdigit()) {
        return Err("mirror export nonce must be 8–32 hex characters".into());
    }
    Ok(format!("kcore-{vm}-{nonce}"))
}

pub fn random_nonce() -> String {
    use std::io::Read;
    let mut buf = [0u8; 8];
    if let Ok(mut file) = fs::File::open("/dev/urandom") {
        let _ = file.read_exact(&mut buf);
    }
    // urandom is the normal source. If it cannot be read, mix the time so two
    // exports in the same process still differ; the name is not a secret.
    if buf.iter().all(|b| *b == 0) {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        buf.copy_from_slice(&nanos.to_le_bytes()[..8]);
    }
    buf.iter().map(|b| format!("{b:02x}")).collect()
}

pub fn parse_bind_addr(addr: &str) -> Result<IpAddr, String> {
    let addr = addr.trim();
    addr.parse::<IpAddr>()
        .map_err(|_| format!("bind address '{addr}' is not an IP address"))
}

/// `dmsetup` table line for a linear map of `sectors` over `backing`.
pub fn linear_table(sectors: u64, backing: &str) -> Result<String, String> {
    if sectors == 0 {
        return Err("device mapper length must be non-zero".into());
    }
    let backing = require_table_device(backing)?;
    Ok(format!("0 {sectors} linear {backing} 0\n"))
}

/// `dmsetup` table line for a full resync onto `leg1`.
///
/// A fresh core log starts with every region out of sync, which is what we
/// want: `nosync` would claim the empty destination already matched.
pub fn mirror_table(sectors: u64, leg0: &str, leg1: &str) -> Result<String, String> {
    if sectors == 0 {
        return Err("device mapper length must be non-zero".into());
    }
    let leg0 = require_dev_path(leg0)?;
    let leg1 = require_dev_path(leg1)?;
    let region = mirror_region_sectors(sectors)?;
    Ok(format!(
        "0 {sectors} mirror core 1 {region} 2 {leg0} 0 {leg1} 0\n"
    ))
}

/// Largest power-of-two region that `dm-mirror` will load for this device.
fn mirror_region_sectors(device_sectors: u64) -> Result<u64, String> {
    if device_sectors < MIN_MIRROR_REGION_SECTORS {
        return Err(format!(
            "device is {device_sectors} sectors; dm-mirror needs at least {MIN_MIRROR_REGION_SECTORS}"
        ));
    }
    let capped = MIRROR_REGION_SECTORS.min(device_sectors);
    let region = if capped.is_power_of_two() {
        capped
    } else {
        capped.next_power_of_two() / 2
    };
    if region < MIN_MIRROR_REGION_SECTORS {
        return Err(format!(
            "device is {device_sectors} sectors; no power-of-two mirror region fits"
        ));
    }
    Ok(region)
}

fn require_dev_path(path: &str) -> Result<&str, String> {
    let path = path.trim();
    if !path.starts_with("/dev/") || path.contains([' ', '\n', '\t', ';', '|', '$', '`']) {
        return Err(format!("refusing device path '{path}'"));
    }
    Ok(path)
}

/// A device-mapper table leg is either a `/dev` path or the `major:minor`
/// pair `dmsetup table` prints back.
fn require_table_device(spec: &str) -> Result<&str, String> {
    let spec = spec.trim();
    if let Some((major, minor)) = spec.split_once(':') {
        if !major.is_empty()
            && !minor.is_empty()
            && major.chars().all(|c| c.is_ascii_digit())
            && minor.chars().all(|c| c.is_ascii_digit())
        {
            return Ok(spec);
        }
    }
    require_dev_path(spec)
}

/// Rebuild the linear table from a mirror table, keeping leg 0 (the local disk).
pub fn linear_table_from_mirror_table(table: &str) -> Result<String, String> {
    let fields: Vec<&str> = table.split_whitespace().collect();
    // 0 LEN mirror core 1 REGION 2 LEG0 0 LEG1 0
    if fields.len() < 10 || fields.first() != Some(&"0") || fields.get(2) != Some(&"mirror") {
        return Err(format!("not a mirror table: {table}"));
    }
    let sectors: u64 = fields[1]
        .parse()
        .map_err(|_| format!("mirror length is not a number: {}", fields[1]))?;
    let leg0 = fields[7];
    linear_table(sectors, leg0)
}

/// Parse one `dmsetup status <name>` line.
pub fn parse_dmsetup_status(line: &str) -> Result<DmStatus, String> {
    let fields: Vec<&str> = line.split_whitespace().collect();
    // `dmsetup status` for linear is `START LEN linear`. The table line is
    // longer (`START LEN linear DEV OFFSET`); both are the same target.
    if fields.len() < 3 {
        return Err(format!("dmsetup status line is too short: {line}"));
    }
    let sectors: u64 = fields[1]
        .parse()
        .map_err(|_| format!("dmsetup length is not a number: {}", fields[1]))?;
    match fields[2] {
        "linear" => Ok(DmStatus::Linear { sectors }),
        "mirror" => {
            if fields.len() < 4 {
                return Err(format!("mirror status is truncated: {line}"));
            }
            let nr: usize = fields[3]
                .parse()
                .map_err(|_| format!("mirror leg count is not a number: {}", fields[3]))?;
            if nr == 0 || fields.len() < 6 + nr {
                return Err(format!("mirror status is truncated: {line}"));
            }
            let sync_field = fields[4 + nr];
            let health = fields[5 + nr];
            let (synced, total) = sync_field
                .split_once('/')
                .ok_or_else(|| format!("mirror sync field '{sync_field}' is not synced/total"))?;
            let synced: u64 = synced
                .parse()
                .map_err(|_| format!("synced region count '{synced}' is not a number"))?;
            let total: u64 = total
                .parse()
                .map_err(|_| format!("region count '{total}' is not a number"))?;
            let bytes_total = sectors.saturating_mul(512);
            let bytes_synced = bytes_total
                .saturating_mul(synced)
                .checked_div(total)
                .unwrap_or(0);
            let healthy = health.len() == nr && health.chars().all(|c| c == 'A');
            let degraded = health
                .chars()
                .any(|c| matches!(c, 'D' | 'F' | 'S' | 'R' | 'U'));
            let in_sync = total > 0 && synced == total && healthy;
            Ok(DmStatus::Mirror(MirrorProgress {
                sectors,
                bytes_synced,
                bytes_total,
                in_sync,
                degraded,
            }))
        }
        other => Err(format!("unsupported device-mapper target '{other}'")),
    }
}

pub fn guest_disk_facts(
    state: Option<&str>,
    disk_paths: &[String],
    memory_shared: bool,
    mapper: &str,
) -> GuestDiskFacts {
    let running = matches!(state, Some("Running") | Some("Paused"));
    GuestDiskFacts {
        running,
        guest_uses_mapper: disk_paths.iter().any(|p| p == mapper),
        memory_shared,
    }
}

pub fn sectors_for_bytes(size_bytes: u64) -> Result<u64, String> {
    if size_bytes == 0 || !size_bytes.is_multiple_of(512) {
        return Err(format!(
            "volume size {size_bytes} is not a positive multiple of 512"
        ));
    }
    Ok(size_bytes / 512)
}

pub fn qemu_nbd_argv(
    bind: IpAddr,
    port: u16,
    export: &str,
    device: &str,
    pid_file: &Path,
) -> Result<Vec<String>, String> {
    let export = validate_export_name(export)?;
    let device = require_dev_path(device)?.to_string();
    if !(NBD_PORT_BASE..NBD_PORT_BASE + NBD_PORT_COUNT).contains(&port) {
        return Err(format!("NBD port {port} is outside the mirror range"));
    }
    Ok(vec![
        "qemu-nbd".into(),
        "--persistent".into(),
        "--shared=1".into(),
        "--cache=none".into(),
        "--aio=native".into(),
        "--fork".into(),
        "--pid-file".into(),
        pid_file.display().to_string(),
        "-b".into(),
        bind.to_string(),
        "-p".into(),
        port.to_string(),
        "-x".into(),
        export,
        "-f".into(),
        "raw".into(),
        device,
    ])
}

pub fn nbd_client_argv(
    host: &str,
    port: u16,
    device: &str,
    export: &str,
) -> Result<Vec<String>, String> {
    let host = host.trim();
    if host.is_empty() || host.contains([' ', '\n', ';']) {
        return Err(format!("refusing NBD host '{host}'"));
    }
    // An address is enough; a hostname would also be fine but the controller
    // passes the gRPC host, which is an IP.
    if host.parse::<IpAddr>().is_err() {
        return Err(format!("NBD host '{host}' is not an IP address"));
    }
    let device = require_dev_path(device)?.to_string();
    let export = validate_export_name(export)?;
    Ok(vec![
        "nbd-client".into(),
        host.to_string(),
        port.to_string(),
        device,
        "-N".into(),
        export,
        "-persist".into(),
        "-timeout".into(),
        "30".into(),
    ])
}

pub fn nbd_disconnect_argv(device: &str) -> Result<Vec<String>, String> {
    let device = require_dev_path(device)?.to_string();
    Ok(vec!["nbd-client".into(), "-d".into(), device])
}

pub fn qemu_img_convert_argv(
    src: &str,
    host: &str,
    port: u16,
    export: &str,
) -> Result<Vec<String>, String> {
    let src = require_dev_path(src)?.to_string();
    let host = host.trim();
    if host.parse::<IpAddr>().is_err() {
        return Err(format!("NBD host '{host}' is not an IP address"));
    }
    let export = validate_export_name(export)?;
    let host_in_url = if host.contains(':') {
        format!("[{host}]")
    } else {
        host.to_string()
    };
    Ok(vec![
        "qemu-img".into(),
        "convert".into(),
        "-f".into(),
        "raw".into(),
        "-O".into(),
        "raw".into(),
        "-t".into(),
        "none".into(),
        "-n".into(),
        src,
        format!("nbd://{host_in_url}:{port}/{export}"),
    ])
}

pub fn lvcreate_argv(vg: &str, vm: &str, size_bytes: u64) -> Result<Vec<String>, String> {
    let vg = validate_storage_name(vg, "volume group")?;
    let lv = lv_name(vm)?;
    if size_bytes == 0 {
        return Err("refusing to create a zero-length logical volume".into());
    }
    Ok(vec![
        "lvcreate".into(),
        "-y".into(),
        "-L".into(),
        format!("{size_bytes}B"),
        "-n".into(),
        lv,
        vg,
    ])
}

pub fn lvremove_argv(backing: &str) -> Result<Vec<String>, String> {
    let backing = require_dev_path(backing)?.to_string();
    Ok(vec!["lvremove".into(), "-f".into(), backing])
}

pub fn zfs_create_argv(pool: &str, vm: &str, size_bytes: u64) -> Result<Vec<String>, String> {
    let dataset = zfs_dataset(pool, vm)?;
    if size_bytes == 0 {
        return Err("refusing to create a zero-length zvol".into());
    }
    Ok(vec![
        "zfs".into(),
        "create".into(),
        "-V".into(),
        size_bytes.to_string(),
        "-o".into(),
        "volmode=dev".into(),
        dataset,
    ])
}

pub fn zfs_destroy_argv(pool: &str, vm: &str) -> Result<Vec<String>, String> {
    Ok(vec![
        "zfs".into(),
        "destroy".into(),
        "-f".into(),
        zfs_dataset(pool, vm)?,
    ])
}

pub fn load_session(socket_dir: &Path, vm: &str) -> Result<Option<MirrorSession>, String> {
    let path = session_path(socket_dir, vm)?;
    if !path.exists() {
        return Ok(None);
    }
    let raw = fs::read_to_string(&path).map_err(|e| format!("read {}: {e}", path.display()))?;
    serde_json::from_str(&raw).map(Some).map_err(|e| {
        format!(
            "volume-mirror session {} is not readable: {e}",
            path.display()
        )
    })
}

pub fn store_session(socket_dir: &Path, session: &MirrorSession) -> Result<(), String> {
    let path = session_path(socket_dir, &session.vm_name)?;
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir).map_err(|e| format!("create {}: {e}", dir.display()))?;
    }
    let raw = serde_json::to_string(session).map_err(|e| format!("encode session: {e}"))?;
    fs::write(&path, raw).map_err(|e| format!("write {}: {e}", path.display()))
}

pub fn clear_session(socket_dir: &Path, vm: &str) -> Result<(), String> {
    let path = session_path(socket_dir, vm)?;
    if path.exists() {
        fs::remove_file(&path).map_err(|e| format!("remove {}: {e}", path.display()))?;
    }
    Ok(())
}

fn run(argv: &[String]) -> Result<(), String> {
    let (bin, args) = argv
        .split_first()
        .ok_or_else(|| "empty command".to_string())?;
    let out = Command::new(bin)
        .args(args)
        .output()
        .map_err(|e| format!("run {bin}: {e}"))?;
    if out.status.success() {
        return Ok(());
    }
    let stderr = String::from_utf8_lossy(&out.stderr);
    let stdout = String::from_utf8_lossy(&out.stdout);
    Err(format!(
        "{bin} {} failed: {}{}",
        args.join(" "),
        stderr.trim(),
        stdout.trim()
    ))
}

fn capture(argv: &[String]) -> Result<String, String> {
    let (bin, args) = argv
        .split_first()
        .ok_or_else(|| "empty command".to_string())?;
    let out = Command::new(bin)
        .args(args)
        .output()
        .map_err(|e| format!("run {bin}: {e}"))?;
    if !out.status.success() {
        return Err(format!(
            "{bin} {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

fn run_with_stdin(argv: &[String], stdin_text: &str) -> Result<(), String> {
    let (bin, args) = argv
        .split_first()
        .ok_or_else(|| "empty command".to_string())?;
    let mut child = Command::new(bin)
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("run {bin}: {e}"))?;
    if let Some(mut stdin) = child.stdin.take() {
        stdin
            .write_all(stdin_text.as_bytes())
            .map_err(|e| format!("write {bin} stdin: {e}"))?;
    }
    let out = child
        .wait_with_output()
        .map_err(|e| format!("wait {bin}: {e}"))?;
    if out.status.success() {
        return Ok(());
    }
    Err(format!(
        "{bin} {} failed: {}",
        args.join(" "),
        String::from_utf8_lossy(&out.stderr).trim()
    ))
}

pub fn device_bytes(path: &str) -> Result<u64, String> {
    let path = require_dev_path(path)?.to_string();
    let text = capture(&["blockdev".into(), "--getsize64".into(), path])?;
    text.parse::<u64>()
        .map_err(|_| format!("blockdev size '{text}' is not a number"))
}

pub fn dm_status(vm: &str) -> Result<DmStatus, String> {
    let name = mapper_name(vm)?;
    let text = capture(&["dmsetup".into(), "status".into(), name])?;
    // `dmsetup status <name>` prints the row without the name.
    parse_dmsetup_status(text.lines().next().unwrap_or(text.as_str()))
}

pub fn dm_table(vm: &str) -> Result<String, String> {
    let name = mapper_name(vm)?;
    capture(&["dmsetup".into(), "table".into(), name])
}

fn dm_create(vm: &str, table: &str) -> Result<(), String> {
    let name = mapper_name(vm)?;
    run_with_stdin(&["dmsetup".into(), "create".into(), name], table)
}

fn dm_reload(vm: &str, table: &str) -> Result<(), String> {
    let name = mapper_name(vm)?;
    run(&["dmsetup".into(), "suspend".into(), name.clone()])?;
    let loaded = run_with_stdin(&["dmsetup".into(), "load".into(), name.clone()], table);
    let resumed = run(&["dmsetup".into(), "resume".into(), name]);
    loaded?;
    resumed
}

pub fn dm_remove(vm: &str) -> Result<(), String> {
    let name = mapper_name(vm)?;
    if capture(&["dmsetup".into(), "status".into(), name.clone()]).is_err() {
        return Ok(());
    }
    run(&["dmsetup".into(), "remove".into(), name])
}

fn ensure_mapper(vm: &str, sectors: u64, backing: &str) -> Result<(), String> {
    let path = mapper_path(vm)?;
    if Path::new(&path).exists() {
        return Ok(());
    }
    let _ = run(&["modprobe".into(), "dm-mod".into()]);
    dm_create(vm, &linear_table(sectors, backing)?)
}

fn wait_for_block(path: &str) -> Result<(), String> {
    let deadline = std::time::Instant::now() + Duration::from_secs(30);
    loop {
        if Path::new(path).exists() {
            return Ok(());
        }
        if std::time::Instant::now() >= deadline {
            return Err(format!("{path} did not appear within 30s"));
        }
        std::thread::sleep(Duration::from_millis(200));
    }
}

fn create_backing(
    backend: &str,
    vg: &str,
    pool: &str,
    vm: &str,
    size_bytes: u64,
) -> Result<String, String> {
    let path = backing_path(backend, vg, pool, vm)?;
    if Path::new(&path).exists() {
        return Ok(path);
    }
    match backend {
        "lvm" => run(&lvcreate_argv(vg, vm, size_bytes)?)?,
        "zfs" => {
            run(&zfs_create_argv(pool, vm, size_bytes)?)?;
        }
        other => return Err(format!("unsupported backend {other}")),
    }
    wait_for_block(&path)?;
    Ok(path)
}

fn destroy_backing(session: &MirrorSession) -> Result<(), String> {
    match session.backend.as_str() {
        "lvm" => {
            if Path::new(&session.backing_path).exists() {
                run(&lvremove_argv(&session.backing_path)?)?;
            }
        }
        "zfs" => {
            let argv = zfs_destroy_argv(&session.pool_name, &session.vm_name)?;
            // A missing dataset is the idempotent case.
            let _ = run(&argv);
        }
        other => return Err(format!("unsupported backend {other}")),
    }
    if let Ok(marker) = seed_marker_path(&session.vm_name) {
        let _ = fs::remove_file(marker);
    }
    Ok(())
}

/// Create the destination LV/zvol, the stable mapper, and the NBD export.
pub fn prepare_target(
    socket_dir: &Path,
    vm: &str,
    backend: &str,
    vg: &str,
    pool: &str,
    size_bytes: u64,
    bind: IpAddr,
) -> Result<MirrorSession, String> {
    let vm = validate_vm_name(vm)?;
    let backend = match backend.trim() {
        "lvm" | "zfs" => backend.trim().to_string(),
        other => return Err(format!("volume mirror supports lvm or zfs, not {other}")),
    };
    let sectors = sectors_for_bytes(size_bytes)?;
    if let Some(existing) = load_session(socket_dir, &vm)? {
        if existing.is_target() && existing.nbd_pid != 0 && pid_alive(existing.nbd_pid) {
            return Ok(existing);
        }
    }
    let port = reserve_nbd_port()?;
    let export = export_name(&vm, &random_nonce())?;
    let pid_path = nbd_pid_path(socket_dir, &vm)?;
    let backing = backing_path(&backend, vg, pool, &vm)?;
    let mut session = MirrorSession {
        role: "target".into(),
        vm_name: vm.clone(),
        backend,
        vg_name: vg.trim().to_string(),
        pool_name: pool.trim().to_string(),
        export_name: export.clone(),
        port,
        bind_addr: bind.to_string(),
        nbd_device: String::new(),
        backing_path: backing.clone(),
        mapper_path: mapper_path(&vm)?,
        nbd_pid: 0,
        size_bytes,
    };
    // Record the intent before creating anything, so an abort still knows this
    // volume belongs to a mirror and may be deleted.
    store_session(socket_dir, &session)?;
    let backing = create_backing(&session.backend, vg, pool, &vm, size_bytes)?;
    session.backing_path = backing.clone();
    let actual = device_bytes(&backing)?;
    if actual < size_bytes {
        return Err(format!(
            "{backing} is {actual} bytes, smaller than the source volume ({size_bytes})"
        ));
    }
    ensure_mapper(&vm, sectors, &backing)?;
    store_session(socket_dir, &session)?;
    let argv = qemu_nbd_argv(bind, port, &export, &backing, &pid_path)?;
    run(&argv)?;
    let pid = fs::read_to_string(&pid_path)
        .ok()
        .and_then(|s| s.trim().parse().ok())
        .unwrap_or(0);
    let session = MirrorSession {
        nbd_pid: pid,
        ..session
    };
    store_session(socket_dir, &session)?;
    Ok(session)
}

fn reserve_nbd_port() -> Result<u16, String> {
    for offset in 0..NBD_PORT_COUNT {
        let port = NBD_PORT_BASE + offset;
        if std::net::TcpListener::bind((std::net::Ipv4Addr::UNSPECIFIED, port)).is_ok() {
            // Drop the listener immediately. qemu-nbd binds next; the window
            // is the same one the migration port reservation already accepts.
            return Ok(port);
        }
    }
    Err(format!(
        "no free NBD port in {NBD_PORT_BASE}-{}",
        NBD_PORT_BASE + NBD_PORT_COUNT - 1
    ))
}

fn pid_alive(pid: u32) -> bool {
    if pid == 0 {
        return false;
    }
    unsafe { libc::kill(pid as i32, 0) == 0 }
}

fn free_nbd_device() -> Result<String, String> {
    let _ = run(&["modprobe".into(), "nbd".into(), "nbds_max=64".into()]);
    for i in 0..64 {
        let dev = format!("/dev/nbd{i}");
        if !Path::new(&dev).exists() {
            continue;
        }
        let size_path = format!("/sys/block/nbd{i}/size");
        let size = fs::read_to_string(&size_path).unwrap_or_default();
        if size.trim() == "0" {
            return Ok(dev);
        }
    }
    Err("no free /dev/nbd device (modprobe nbd nbds_max=64)".into())
}

/// Destination NBD export a mirror leg or a cold copy connects to.
pub struct NbdTarget<'a> {
    pub host: &'a str,
    pub port: u16,
    pub export: &'a str,
}

/// Point the running guest's mapper at the destination export and resync.
pub fn start_mirror(
    socket_dir: &Path,
    vm: &str,
    backend: &str,
    vg: &str,
    pool: &str,
    target: NbdTarget<'_>,
) -> Result<MirrorSession, String> {
    let vm = validate_vm_name(vm)?;
    let export = validate_export_name(target.export)?;
    let backing = backing_path(backend, vg, pool, &vm)?;
    let mapper = mapper_path(&vm)?;
    if !Path::new(&mapper).exists() {
        return Err(format!(
            "{mapper} does not exist; the guest is not on the stable mapper path yet. \
             Restart the VM once so its unit creates it, then migrate"
        ));
    }
    match dm_status(&vm)? {
        DmStatus::Linear { .. } => {}
        DmStatus::Mirror(_) => {
            return Err(format!(
                "{mapper} is already a mirror; abort the previous attempt before starting another"
            ));
        }
    }
    let sectors = sectors_for_bytes(device_bytes(&mapper)?)?;
    let nbd = free_nbd_device()?;
    run(&nbd_client_argv(target.host, target.port, &nbd, &export)?)?;
    let nbd_bytes = device_bytes(&nbd).inspect_err(|_| {
        let _ = run(&nbd_disconnect_argv(&nbd).unwrap_or_default());
    })?;
    if nbd_bytes < sectors * 512 {
        let _ = run(&nbd_disconnect_argv(&nbd)?);
        return Err(format!(
            "destination export is {nbd_bytes} bytes, smaller than the source ({})",
            sectors * 512
        ));
    }
    let table = mirror_table(sectors, &backing, &nbd)?;
    dm_reload(&vm, &table).inspect_err(|_| {
        let _ = run(&nbd_disconnect_argv(&nbd).unwrap_or_default());
    })?;
    let session = MirrorSession {
        role: "source".into(),
        vm_name: vm,
        backend: backend.trim().to_string(),
        vg_name: vg.trim().to_string(),
        pool_name: pool.trim().to_string(),
        export_name: export,
        port: target.port,
        bind_addr: target.host.trim().to_string(),
        nbd_device: nbd,
        backing_path: backing,
        mapper_path: mapper,
        nbd_pid: 0,
        size_bytes: sectors * 512,
    };
    store_session(socket_dir, &session)?;
    Ok(session)
}

pub fn mirror_status(vm: &str) -> Result<MirrorProgress, String> {
    match dm_status(vm)? {
        DmStatus::Mirror(progress) => Ok(progress),
        DmStatus::Linear { sectors } => Ok(MirrorProgress {
            sectors,
            bytes_synced: 0,
            bytes_total: sectors.saturating_mul(512),
            in_sync: false,
            degraded: false,
        }),
    }
}

/// Full copy of a quiescent disk. The guest must already be stopped.
///
/// On success the source records a session so a later `destroy_source_copy`
/// can delete this disk. Until then an abort leaves it in place.
pub fn copy_quiescent(
    socket_dir: &Path,
    vm: &str,
    backend: &str,
    vg: &str,
    pool: &str,
    target: NbdTarget<'_>,
) -> Result<(), String> {
    let vm = validate_vm_name(vm)?;
    let backing = backing_path(backend, vg, pool, &vm)?;
    let mapper = mapper_path(&vm)?;
    run(&qemu_img_convert_argv(
        &backing,
        target.host,
        target.port,
        target.export,
    )?)?;
    let session = MirrorSession {
        role: "source".into(),
        vm_name: vm,
        backend: backend.trim().to_string(),
        vg_name: vg.trim().to_string(),
        pool_name: pool.trim().to_string(),
        export_name: validate_export_name(target.export)?,
        port: target.port,
        bind_addr: target.host.trim().to_string(),
        nbd_device: String::new(),
        backing_path: backing,
        mapper_path: mapper,
        nbd_pid: 0,
        size_bytes: 0,
    };
    store_session(socket_dir, &session)
}

pub fn write_seed_marker(vm: &str) -> Result<(), String> {
    let path = seed_marker_path(vm)?;
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir).map_err(|e| format!("create {}: {e}", dir.display()))?;
    }
    fs::write(&path, b"1").map_err(|e| format!("write {}: {e}", path.display()))
}

fn stop_nbd_export(socket_dir: &Path, session: &MirrorSession) -> Result<(), String> {
    let pid_path = nbd_pid_path(socket_dir, &session.vm_name)?;
    let pid = if session.nbd_pid != 0 {
        session.nbd_pid
    } else {
        fs::read_to_string(&pid_path)
            .ok()
            .and_then(|s| s.trim().parse().ok())
            .unwrap_or(0)
    };
    if pid_alive(pid) {
        unsafe {
            libc::kill(pid as i32, libc::SIGTERM);
        }
        for _ in 0..50 {
            if !pid_alive(pid) {
                break;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        if pid_alive(pid) {
            unsafe {
                libc::kill(pid as i32, libc::SIGKILL);
            }
        }
    }
    let _ = fs::remove_file(pid_path);
    Ok(())
}

fn restore_source_linear(session: &MirrorSession) -> Result<(), String> {
    if !session.nbd_device.is_empty() {
        let _ = run(&nbd_disconnect_argv(&session.nbd_device).unwrap_or_default());
    }
    // An empty path is the current directory, which must not be treated as a mapper.
    if !session.mapper_path.is_empty() && Path::new(&session.mapper_path).exists() {
        if let Ok(table) = dm_table(&session.vm_name) {
            if table.split_whitespace().nth(2) == Some("mirror") {
                let linear = linear_table_from_mirror_table(&table)?;
                dm_reload(&session.vm_name, &linear)?;
            }
        }
    }
    Ok(())
}

/// Tear down a mirror session.
///
/// `destroy_volume` deletes the destination copy. It is honored only for a
/// session this node recorded as the target, so a retry cannot remove the
/// source disk the guest is using.
pub fn abort_session(socket_dir: &Path, vm: &str, destroy_volume: bool) -> Result<String, String> {
    let vm = validate_vm_name(vm)?;
    let Some(session) = load_session(socket_dir, &vm)? else {
        return Ok(format!("no volume-mirror session for {vm}"));
    };
    let message = if session.is_source() {
        restore_source_linear(&session)?;
        format!("restored {vm} to a linear mapping")
    } else {
        stop_nbd_export(socket_dir, &session)?;
        let _ = dm_remove(&vm);
        if destroy_volume {
            destroy_backing(&session)?;
            format!("removed mirror target for {vm}")
        } else {
            format!("stopped NBD export for {vm}")
        }
    };
    clear_session(socket_dir, &vm)?;
    Ok(message)
}

/// After a successful move: drop the source copy.
pub fn destroy_source_copy(socket_dir: &Path, vm: &str) -> Result<(), String> {
    let vm = validate_vm_name(vm)?;
    let Some(session) = load_session(socket_dir, &vm)? else {
        return Ok(());
    };
    if !session.is_source() {
        return Err(format!(
            "refusing to delete the volume for {vm}: this node is the mirror target, not the source"
        ));
    }
    restore_source_linear(&session)?;
    let _ = dm_remove(&vm);
    destroy_backing(&session)?;
    clear_session(socket_dir, &vm)?;
    Ok(())
}

/// After a successful move: the destination keeps the volume and drops the export.
pub fn release_target_export(socket_dir: &Path, vm: &str) -> Result<(), String> {
    let vm = validate_vm_name(vm)?;
    let Some(session) = load_session(socket_dir, &vm)? else {
        return Ok(());
    };
    if !session.is_target() {
        return Err(format!(
            "refusing to drop the NBD export for {vm}: this node is the mirror source"
        ));
    }
    stop_nbd_export(socket_dir, &session)?;
    clear_session(socket_dir, &vm)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_match_the_nix_unit() {
        assert_eq!(mapper_path("web-1").unwrap(), "/dev/mapper/kcore-vm-web-1");
        assert_eq!(
            lvm_backing("vg_kcore", "web-1").unwrap(),
            "/dev/vg_kcore/kcore-web-1"
        );
        assert_eq!(
            zfs_backing("tank0", "web-1").unwrap(),
            "/dev/zvol/tank0/kcore-web-1"
        );
    }

    #[test]
    fn vm_name_rejects_path_metacharacters() {
        assert!(validate_vm_name("../etc").is_err());
        assert!(validate_vm_name("a b").is_err());
        assert!(validate_vm_name("").is_err());
        assert!(validate_vm_name("ok_name-1").is_ok());
    }

    #[test]
    fn mirror_table_resyncs_and_linear_round_trips() {
        let table = mirror_table(41_943_040, "/dev/vg_kcore/kcore-web", "/dev/nbd3").unwrap();
        assert!(table.contains("mirror core 1 32768 2"));
        assert!(!table.contains("nosync"));
        let back = linear_table_from_mirror_table(&table).unwrap();
        assert_eq!(back, "0 41943040 linear /dev/vg_kcore/kcore-web 0\n");
        // `dmsetup table` prints legs as major:minor, not the path that was loaded.
        let numeric =
            linear_table_from_mirror_table("0 41943040 mirror core 1 1024 2 254:0 0 43:0 0")
                .unwrap();
        assert_eq!(numeric, "0 41943040 linear 254:0 0\n");
    }

    #[test]
    fn mirror_region_shrinks_to_fit_a_short_device() {
        let table = mirror_table(2048, "/dev/vg_kcore/kcore-web", "/dev/nbd3").unwrap();
        assert!(table.contains("mirror core 1 2048 2"));
        let odd = mirror_table(3000, "/dev/vg_kcore/kcore-web", "/dev/nbd0").unwrap();
        assert!(odd.contains("mirror core 1 2048 2"));
        assert!(mirror_table(7, "/dev/vg_kcore/kcore-web", "/dev/nbd0").is_err());
    }

    #[test]
    fn device_paths_with_shell_metacharacters_are_refused() {
        assert!(linear_table(10, "/dev/mapper/bad;rm").is_err());
        assert!(mirror_table(10, "/tmp/not-dev", "/dev/nbd0").is_err());
    }

    #[test]
    fn mirror_status_reports_partial_and_complete_sync() {
        let partial =
            parse_dmsetup_status("0 2097152 mirror 2 253:0 253:1 100/400 AA 1 core").unwrap();
        match partial {
            DmStatus::Mirror(p) => {
                assert!(!p.in_sync);
                assert!(!p.degraded);
                assert_eq!(p.bytes_total, 2097152 * 512);
                assert_eq!(p.bytes_synced, 2097152 * 512 / 4);
            }
            other => panic!("expected mirror, got {other:?}"),
        }
        let done =
            parse_dmsetup_status("0 2097152 mirror 2 253:0 253:1 400/400 AA 1 core").unwrap();
        match done {
            DmStatus::Mirror(p) => {
                assert!(p.in_sync);
                assert_eq!(p.bytes_synced, p.bytes_total);
            }
            other => panic!("expected mirror, got {other:?}"),
        }
    }

    #[test]
    fn a_failed_leg_is_not_in_sync() {
        let status = parse_dmsetup_status("0 100 mirror 2 253:0 253:1 100/100 AD 1 core").unwrap();
        match status {
            DmStatus::Mirror(p) => {
                assert!(!p.in_sync);
                assert!(p.degraded);
            }
            other => panic!("expected mirror, got {other:?}"),
        }
    }

    #[test]
    fn linear_status_is_not_a_mirror() {
        assert_eq!(
            parse_dmsetup_status("0 41943040 linear").unwrap(),
            DmStatus::Linear { sectors: 41943040 }
        );
        assert_eq!(
            parse_dmsetup_status("0 2048 linear 253:1 0").unwrap(),
            DmStatus::Linear { sectors: 2048 }
        );
    }

    #[test]
    fn guest_facts_require_the_mapper_and_shared_memory() {
        let mapper = mapper_path("web").unwrap();
        let live = guest_disk_facts(
            Some("Running"),
            std::slice::from_ref(&mapper),
            true,
            &mapper,
        );
        assert!(live.running && live.guest_uses_mapper && live.memory_shared);
        let old = guest_disk_facts(
            Some("Running"),
            &["/dev/vg_kcore/kcore-web".into()],
            false,
            &mapper,
        );
        assert!(old.running && !old.guest_uses_mapper && !old.memory_shared);
        let stopped = guest_disk_facts(None, &[], false, &mapper);
        assert!(!stopped.running);
    }

    #[test]
    fn nbd_argv_stays_inside_the_port_range_and_uses_direct_io() {
        let argv = qemu_nbd_argv(
            "10.0.0.5".parse().unwrap(),
            18128,
            "kcore-web-aabbccdd",
            "/dev/vg_kcore/kcore-web",
            Path::new("/run/kcore/web.nbd.pid"),
        )
        .unwrap();
        assert!(argv.iter().any(|a| a == "--cache=none"));
        assert!(argv.iter().any(|a| a == "--aio=native"));
        assert!(qemu_nbd_argv(
            "10.0.0.5".parse().unwrap(),
            18000,
            "kcore-web-aabbccdd",
            "/dev/vg_kcore/kcore-web",
            Path::new("/run/kcore/web.nbd.pid"),
        )
        .is_err());
    }

    #[test]
    fn qemu_img_url_brackets_ipv6() {
        let argv = qemu_img_convert_argv(
            "/dev/zvol/tank0/kcore-web",
            "2001:db8::5",
            18130,
            "kcore-web-aabbccdd",
        )
        .unwrap();
        assert_eq!(
            argv.last().map(String::as_str),
            Some("nbd://[2001:db8::5]:18130/kcore-web-aabbccdd")
        );
    }

    #[test]
    fn session_round_trip_and_clear() {
        let dir = tempfile::tempdir().unwrap();
        let session = MirrorSession {
            role: "target".into(),
            vm_name: "web".into(),
            backend: "lvm".into(),
            vg_name: "vg_kcore".into(),
            pool_name: String::new(),
            export_name: "kcore-web-aabbccdd".into(),
            port: 18128,
            bind_addr: "10.0.0.5".into(),
            nbd_device: String::new(),
            backing_path: "/dev/vg_kcore/kcore-web".into(),
            mapper_path: "/dev/mapper/kcore-vm-web".into(),
            nbd_pid: 0,
            size_bytes: 1024 * 1024,
        };
        store_session(dir.path(), &session).unwrap();
        assert_eq!(load_session(dir.path(), "web").unwrap(), Some(session));
        clear_session(dir.path(), "web").unwrap();
        assert_eq!(load_session(dir.path(), "web").unwrap(), None);
    }

    #[test]
    fn size_must_be_a_sector_multiple() {
        assert!(sectors_for_bytes(0).is_err());
        assert!(sectors_for_bytes(100).is_err());
        assert_eq!(sectors_for_bytes(4096).unwrap(), 8);
    }
}
