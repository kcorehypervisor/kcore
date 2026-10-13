//! Host logical-volume operations for the node storage API.
//!
//! VM disks keep the configured name prefix. An explicit `lv_name` creates a
//! volume in the same group without that prefix, then format, mount, and
//! resize can manage it. Command plans are pure so tests do not need LVM.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::path_safety;

const MIB: i64 = 1024 * 1024;
const UNIT_DIR: &str = "/etc/systemd/system";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    InvalidArgument,
    FailedPrecondition,
    Internal,
}

#[derive(Debug)]
pub struct LvmOpError {
    pub kind: Kind,
    pub message: String,
}

impl LvmOpError {
    pub(crate) fn new(kind: Kind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
        }
    }
}

impl std::fmt::Display for LvmOpError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BlockFacts {
    pub size_bytes: i64,
    pub mounted_at: Option<String>,
    pub fs_type: Option<String>,
}

/// Name to pass to `lvcreate -n`. An empty `lv_name` keeps `<prefix><volume_id>`.
pub fn logical_volume_name(
    prefix: &str,
    volume_id: &str,
    lv_name: &str,
) -> Result<String, LvmOpError> {
    let explicit = lv_name.trim();
    if !explicit.is_empty() {
        return validate_lv_name(explicit);
    }
    let id = sanitize_volume_id(volume_id);
    validate_lv_name(&format!("{prefix}{id}"))
}

/// Canonical `/dev/<vg>/<lv>` for a handle that must already live in `vg`.
pub fn logical_volume_device(vg: &str, backend_handle: &str) -> Result<String, LvmOpError> {
    let vg = vg.trim();
    if vg.is_empty() || vg.contains('/') || vg.contains('\\') {
        return Err(LvmOpError::new(
            Kind::FailedPrecondition,
            "node LVM volume group is not configured",
        ));
    }
    let handle = backend_handle.trim();
    let prefix = format!("/dev/{vg}/");
    let name = handle.strip_prefix(&prefix).ok_or_else(|| {
        LvmOpError::new(
            Kind::InvalidArgument,
            format!("logical volume must be under {prefix}"),
        )
    })?;
    let name = validate_lv_name(name)?;
    Ok(format!("{prefix}{name}"))
}

pub fn normalize_fs_type(fs_type: &str) -> Result<String, LvmOpError> {
    match fs_type.trim().to_ascii_lowercase().as_str() {
        "ext4" => Ok("ext4".to_string()),
        "xfs" => Ok("xfs".to_string()),
        other => Err(LvmOpError::new(
            Kind::InvalidArgument,
            format!("fs_type must be ext4 or xfs, got {other}"),
        )),
    }
}

pub fn validate_mount_point(mount_point: &str) -> Result<String, LvmOpError> {
    let raw = mount_point.trim();
    if raw.is_empty() {
        return Err(LvmOpError::new(
            Kind::InvalidArgument,
            "mount_point is required",
        ));
    }
    let roots = [Path::new("/var/lib/kcore"), Path::new("/mnt/kcore")];
    let mut last_err = String::new();
    for root in roots {
        match path_safety::validate_path_under_root(raw, root, "mount_point") {
            Ok(path) => {
                if path == root {
                    return Err(LvmOpError::new(
                        Kind::InvalidArgument,
                        format!("mount_point must be a directory under {}", root.display()),
                    ));
                }
                return Ok(path.display().to_string());
            }
            Err(err) => last_err = err,
        }
    }
    Err(LvmOpError::new(Kind::InvalidArgument, last_err))
}

pub fn mount_unit_name(mount_point: &str) -> String {
    let trimmed = mount_point.trim().trim_start_matches('/');
    let mut out = String::new();
    for byte in trimmed.bytes() {
        if byte == b'/' {
            out.push('-');
        } else if byte.is_ascii_alphanumeric() || byte == b'_' || byte == b':' {
            out.push(byte as char);
        } else {
            out.push_str(&format!("\\x{byte:02x}"));
        }
    }
    if out.is_empty() {
        out.push('-');
    }
    out.push_str(".mount");
    out
}

pub fn mount_unit_contents(device: &str, mount_point: &str, fs_type: &str) -> String {
    format!(
        "[Unit]\nDescription=kcore volume {mount_point}\n\n[Mount]\nWhat={device}\nWhere={mount_point}\nType={fs_type}\nOptions=defaults\n\n[Install]\nWantedBy=multi-user.target\n"
    )
}

pub fn resize_commands(
    device: &str,
    size_bytes: i64,
    allow_shrink: bool,
    facts: &BlockFacts,
) -> Result<(Vec<Vec<String>>, String), LvmOpError> {
    if size_bytes <= 0 || size_bytes % MIB != 0 {
        return Err(LvmOpError::new(
            Kind::InvalidArgument,
            "size_bytes must be a positive multiple of 1 MiB",
        ));
    }
    if size_bytes == facts.size_bytes {
        return Ok((
            Vec::new(),
            format!("{device} is already {size_bytes} bytes"),
        ));
    }
    let shrinking = size_bytes < facts.size_bytes;
    if shrinking && !allow_shrink {
        return Err(LvmOpError::new(
            Kind::FailedPrecondition,
            "shrinking a logical volume requires allow_shrink",
        ));
    }
    if shrinking && facts.mounted_at.is_some() {
        return Err(LvmOpError::new(
            Kind::FailedPrecondition,
            "unmount the logical volume before shrinking it",
        ));
    }
    let fs = facts.fs_type.as_deref().unwrap_or("");
    if shrinking && fs == "xfs" {
        return Err(LvmOpError::new(
            Kind::FailedPrecondition,
            "xfs filesystems cannot shrink",
        ));
    }
    if shrinking && !fs.is_empty() && fs != "ext4" {
        return Err(LvmOpError::new(
            Kind::FailedPrecondition,
            format!("cannot shrink a {fs} filesystem"),
        ));
    }

    let size = format!("{size_bytes}B");
    let mib = size_bytes / MIB;
    let mut commands = Vec::new();
    if shrinking && fs == "ext4" {
        commands.push(vec![
            "e2fsck".into(),
            "-f".into(),
            "-y".into(),
            device.into(),
        ]);
        commands.push(vec!["resize2fs".into(), device.into(), format!("{mib}M")]);
        commands.push(vec![
            "lvreduce".into(),
            "-y".into(),
            "-L".into(),
            size,
            device.into(),
        ]);
        return Ok((commands, format!("shrunk {device} to {size_bytes} bytes")));
    }
    if shrinking {
        commands.push(vec![
            "lvreduce".into(),
            "-y".into(),
            "-L".into(),
            size,
            device.into(),
        ]);
        return Ok((commands, format!("shrunk {device} to {size_bytes} bytes")));
    }

    commands.push(vec![
        "lvextend".into(),
        "-y".into(),
        "-L".into(),
        size.clone(),
        device.into(),
    ]);
    let message = match (fs, facts.mounted_at.as_deref()) {
        ("ext4", _) => {
            commands.push(vec!["resize2fs".into(), device.into()]);
            format!("grew {device} to {size_bytes} bytes")
        }
        ("xfs", Some(mount)) => {
            commands.push(vec!["xfs_growfs".into(), mount.into()]);
            format!("grew {device} to {size_bytes} bytes")
        }
        ("xfs", None) => format!(
            "grew {device} to {size_bytes} bytes; mount it and run xfs_growfs to use the new space"
        ),
        ("", _) => format!("grew {device} to {size_bytes} bytes"),
        (other, _) => format!("grew {device} to {size_bytes} bytes; {other} was left unchanged"),
    };
    Ok((commands, message))
}

pub fn mount_fs_type(requested: &str, found: Option<&str>) -> Result<String, LvmOpError> {
    if requested.trim().is_empty() {
        let found = found.ok_or_else(|| {
            LvmOpError::new(
                Kind::FailedPrecondition,
                "volume has no filesystem; format it or pass fs_type",
            )
        })?;
        return normalize_fs_type(found);
    }
    let requested = normalize_fs_type(requested)?;
    if let Some(found) = found {
        let found = normalize_fs_type(found)?;
        if found != requested {
            return Err(LvmOpError::new(
                Kind::InvalidArgument,
                format!("volume has a {found} filesystem, not {requested}"),
            ));
        }
    }
    Ok(requested)
}

pub fn format_commands(
    device: &str,
    fs_type: &str,
    force: bool,
    facts: &BlockFacts,
) -> Result<Vec<String>, LvmOpError> {
    if facts.mounted_at.is_some() {
        return Err(LvmOpError::new(
            Kind::FailedPrecondition,
            "unmount the volume before formatting it",
        ));
    }
    if facts.fs_type.is_some() && !force {
        return Err(LvmOpError::new(
            Kind::FailedPrecondition,
            format!(
                "{device} already has a {} filesystem; pass force to replace it",
                facts.fs_type.as_deref().unwrap_or("existing")
            ),
        ));
    }
    let fs = normalize_fs_type(fs_type)?;
    let mut cmd = vec![format!("mkfs.{fs}")];
    if fs == "ext4" {
        cmd.push("-F".into());
    } else if force {
        cmd.push("-f".into());
    }
    cmd.push(device.into());
    Ok(cmd)
}

pub fn resize_volume(
    vg: &str,
    backend_handle: &str,
    size_bytes: i64,
    allow_shrink: bool,
) -> Result<String, LvmOpError> {
    let device = logical_volume_device(vg, backend_handle)?;
    let facts = probe_device(&device)?;
    let (commands, message) = resize_commands(&device, size_bytes, allow_shrink, &facts)?;
    run_plan(&commands)?;
    Ok(message)
}

pub fn format_volume(
    vg: &str,
    backend_handle: &str,
    fs_type: &str,
    force: bool,
) -> Result<String, LvmOpError> {
    let device = logical_volume_device(vg, backend_handle)?;
    format_device(&device, fs_type, force)
}

pub(crate) fn format_device(
    device: &str,
    fs_type: &str,
    force: bool,
) -> Result<String, LvmOpError> {
    let (mounted_at, fs_found) = block_fs(device)?;
    let facts = BlockFacts {
        size_bytes: 0,
        mounted_at,
        fs_type: fs_found,
    };
    let cmd = format_commands(device, fs_type, force, &facts)?;
    run_argv(&cmd)?;
    Ok(format!(
        "formatted {device} as {}",
        normalize_fs_type(fs_type)?
    ))
}

pub fn mount_volume(
    vg: &str,
    backend_handle: &str,
    mount_point: &str,
    fs_type: &str,
    persistent: bool,
) -> Result<String, LvmOpError> {
    let device = logical_volume_device(vg, backend_handle)?;
    mount_device(&device, mount_point, fs_type, persistent)
}

pub(crate) fn mount_device(
    device: &str,
    mount_point: &str,
    fs_type: &str,
    persistent: bool,
) -> Result<String, LvmOpError> {
    let mount_point = validate_mount_point(mount_point)?;
    let (mounted_at, fs_found) = block_fs(device)?;
    let facts = BlockFacts {
        size_bytes: 0,
        mounted_at,
        fs_type: fs_found,
    };
    if let Some(existing) = facts.mounted_at.as_deref() {
        if existing == mount_point {
            if persistent {
                install_mount_unit(
                    device,
                    &mount_point,
                    facts.fs_type.as_deref().unwrap_or("ext4"),
                )?;
            }
            return Ok(format!("{device} is mounted at {mount_point}"));
        }
        return Err(LvmOpError::new(
            Kind::FailedPrecondition,
            format!("{device} is already mounted at {existing}"),
        ));
    }
    let fs = mount_fs_type(fs_type, facts.fs_type.as_deref())?;
    prepare_mount_point(&mount_point)?;
    if persistent {
        install_mount_unit(device, &mount_point, &fs)?;
    } else {
        run_argv(&[
            "mount".into(),
            "-t".into(),
            fs.clone(),
            device.to_string(),
            mount_point.clone(),
        ])?;
    }
    Ok(format!("mounted {device} at {mount_point}"))
}

pub fn unmount_volume(mount_point: &str, persistent: bool) -> Result<String, LvmOpError> {
    let mount_point = validate_mount_point(mount_point)?;
    if persistent {
        let unit = mount_unit_name(&mount_point);
        let unit_path = Path::new(UNIT_DIR).join(&unit);
        if unit_path.exists() {
            let _ = run_argv(&[
                "systemctl".into(),
                "disable".into(),
                "--now".into(),
                unit.clone(),
            ]);
            fs::remove_file(&unit_path).map_err(|err| {
                LvmOpError::new(
                    Kind::Internal,
                    format!("removing {}: {err}", unit_path.display()),
                )
            })?;
            run_argv(&["systemctl".into(), "daemon-reload".into()])?;
        }
    }
    if mount_point_is_mounted(&mount_point)? {
        run_argv(&["umount".into(), mount_point.clone()])?;
    }
    Ok(format!("unmounted {mount_point}"))
}

pub(crate) fn validate_component(name: &str, what: &str) -> Result<String, LvmOpError> {
    let name = path_safety::validate_safe_segment(name, what)
        .map_err(|err| LvmOpError::new(Kind::InvalidArgument, err))?;
    let mut chars = name.chars();
    let first_ok = chars.next().is_some_and(|c| c.is_ascii_alphanumeric());
    let rest_ok = chars.all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '+' | '-'));
    if !first_ok || !rest_ok {
        return Err(LvmOpError::new(
            Kind::InvalidArgument,
            format!("{what} must be letters, digits, '_', '.', '+', or '-'"),
        ));
    }
    Ok(name.to_string())
}

fn validate_lv_name(name: &str) -> Result<String, LvmOpError> {
    validate_component(name, "lv_name")
}

pub(crate) fn sanitize_volume_id(name: &str) -> String {
    let cleaned: String = name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '-'
            }
        })
        .collect();
    if cleaned.is_empty() {
        "volume".to_string()
    } else {
        cleaned
    }
}

fn install_mount_unit(device: &str, mount_point: &str, fs_type: &str) -> Result<(), LvmOpError> {
    let fs = normalize_fs_type(fs_type)?;
    let unit = mount_unit_name(mount_point);
    let path = PathBuf::from(UNIT_DIR).join(&unit);
    fs::create_dir_all(UNIT_DIR)
        .map_err(|err| LvmOpError::new(Kind::Internal, format!("creating {UNIT_DIR}: {err}")))?;
    fs::write(&path, mount_unit_contents(device, mount_point, &fs)).map_err(|err| {
        LvmOpError::new(Kind::Internal, format!("writing {}: {err}", path.display()))
    })?;
    run_argv(&["systemctl".into(), "daemon-reload".into()])?;
    run_argv(&["systemctl".into(), "enable".into(), "--now".into(), unit])?;
    Ok(())
}

fn prepare_mount_point(mount_point: &str) -> Result<(), LvmOpError> {
    let path = Path::new(mount_point);
    fs::create_dir_all(path)
        .map_err(|err| LvmOpError::new(Kind::Internal, format!("creating {mount_point}: {err}")))?;
    if mount_point_is_mounted(mount_point)? {
        return Ok(());
    }
    let mut entries = fs::read_dir(path)
        .map_err(|err| LvmOpError::new(Kind::Internal, format!("reading {mount_point}: {err}")))?;
    if entries.next().is_some() {
        return Err(LvmOpError::new(
            Kind::FailedPrecondition,
            format!("{mount_point} is not empty"),
        ));
    }
    Ok(())
}

fn mount_point_is_mounted(mount_point: &str) -> Result<bool, LvmOpError> {
    let output = Command::new("findmnt")
        .args(["-n", "-o", "SOURCE", mount_point])
        .output()
        .map_err(|err| LvmOpError::new(Kind::Internal, format!("running findmnt: {err}")))?;
    if output.status.success() {
        return Ok(!String::from_utf8_lossy(&output.stdout).trim().is_empty());
    }
    if output.status.code() == Some(1) {
        return Ok(false);
    }
    Err(LvmOpError::new(
        Kind::Internal,
        format!(
            "findmnt failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ),
    ))
}

fn probe_device(device: &str) -> Result<BlockFacts, LvmOpError> {
    let size_bytes = lvs_size(device)?;
    let (mounted_at, fs_type) = block_fs(device)?;
    Ok(BlockFacts {
        size_bytes,
        mounted_at,
        fs_type,
    })
}

pub(crate) fn block_fs(device: &str) -> Result<(Option<String>, Option<String>), LvmOpError> {
    let (mounted_at, mounted_fs) = findmnt_source(device)?;
    let fs_type = if mounted_fs.is_some() {
        mounted_fs
    } else {
        blkid_type(device)?
    };
    Ok((mounted_at, fs_type))
}

fn lvs_size(device: &str) -> Result<i64, LvmOpError> {
    let stdout = capture(
        "lvs",
        &[
            "--noheadings",
            "--nosuffix",
            "--units",
            "b",
            "-o",
            "lv_size",
            device,
        ],
    )?;
    let token = stdout.split_whitespace().next().unwrap_or("");
    token.parse::<i64>().map_err(|_| {
        LvmOpError::new(
            Kind::Internal,
            format!("could not parse logical volume size from {stdout:?}"),
        )
    })
}

fn findmnt_source(device: &str) -> Result<(Option<String>, Option<String>), LvmOpError> {
    let output = Command::new("findmnt")
        .args(["-n", "-o", "TARGET,FSTYPE", "-S", device])
        .output()
        .map_err(|err| LvmOpError::new(Kind::Internal, format!("running findmnt: {err}")))?;
    if !output.status.success() {
        if output.status.code() == Some(1) {
            return Ok((None, None));
        }
        return Err(LvmOpError::new(
            Kind::Internal,
            format!(
                "findmnt failed: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            ),
        ));
    }
    let line = String::from_utf8_lossy(&output.stdout);
    let mut parts = line.split_whitespace();
    let target = parts.next().map(str::to_string);
    let fs = parts.next().map(str::to_string);
    Ok((target, fs))
}

fn blkid_type(device: &str) -> Result<Option<String>, LvmOpError> {
    let output = Command::new("blkid")
        .args(["-o", "value", "-s", "TYPE", device])
        .output()
        .map_err(|err| LvmOpError::new(Kind::Internal, format!("running blkid: {err}")))?;
    if output.status.success() {
        let fs = String::from_utf8_lossy(&output.stdout).trim().to_string();
        return Ok((!fs.is_empty()).then_some(fs));
    }
    if output.status.code() == Some(2) {
        return Ok(None);
    }
    Err(LvmOpError::new(
        Kind::Internal,
        format!(
            "blkid failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ),
    ))
}

pub(crate) fn run_plan(commands: &[Vec<String>]) -> Result<(), LvmOpError> {
    for cmd in commands {
        run_argv(cmd)?;
    }
    Ok(())
}

pub(crate) fn run_argv(argv: &[String]) -> Result<(), LvmOpError> {
    let (program, args) = argv
        .split_first()
        .ok_or_else(|| LvmOpError::new(Kind::Internal, "empty command"))?;
    capture(
        program,
        &args.iter().map(String::as_str).collect::<Vec<_>>(),
    )
    .map(|_| ())
}

pub(crate) fn capture(program: &str, args: &[&str]) -> Result<String, LvmOpError> {
    let output = Command::new(program).args(args).output().map_err(|err| {
        LvmOpError::new(
            Kind::Internal,
            format!("running {program}: {err}; ensure the storage tools are installed"),
        )
    })?;
    if !output.status.success() {
        return Err(LvmOpError::new(
            Kind::Internal,
            format!(
                "{program} {} failed: {}",
                args.join(" "),
                String::from_utf8_lossy(&output.stderr).trim()
            ),
        ));
    }
    Ok(String::from_utf8_lossy(&output.stdout).to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn facts(size: i64, mounted: Option<&str>, fs: Option<&str>) -> BlockFacts {
        BlockFacts {
            size_bytes: size,
            mounted_at: mounted.map(str::to_string),
            fs_type: fs.map(str::to_string),
        }
    }

    #[test]
    fn explicit_name_skips_the_vm_prefix() {
        let name = logical_volume_name("kcore-", "ignored", "k8s-local").unwrap();
        assert_eq!(name, "k8s-local");
    }

    #[test]
    fn empty_name_keeps_the_vm_prefix() {
        let name = logical_volume_name("kcore-", "win 01", "").unwrap();
        assert_eq!(name, "kcore-win-01");
    }

    #[test]
    fn name_rejects_path_pieces() {
        assert!(logical_volume_name("kcore-", "vm", "../root").is_err());
        assert!(logical_volume_name("kcore-", "vm", "-hidden").is_err());
        assert!(logical_volume_name("kcore-", "vm", "has space").is_err());
    }

    #[test]
    fn device_must_stay_in_the_volume_group() {
        assert_eq!(
            logical_volume_device("vg_kcore", "/dev/vg_kcore/k8s-local").unwrap(),
            "/dev/vg_kcore/k8s-local"
        );
        assert!(logical_volume_device("vg_kcore", "/dev/sda").is_err());
        assert!(logical_volume_device("vg_kcore", "/dev/other/k8s-local").is_err());
        assert!(logical_volume_device("vg_kcore", "/dev/vg_kcore/../sda").is_err());
    }

    #[test]
    fn grow_ext4_extends_the_filesystem() {
        let (commands, message) = resize_commands(
            "/dev/vg_kcore/k8s-local",
            2 * MIB,
            false,
            &facts(MIB, None, Some("ext4")),
        )
        .unwrap();
        assert!(message.contains("grew"));
        assert_eq!(commands[0][0], "lvextend");
        assert_eq!(
            commands[1],
            vec![
                "resize2fs".to_string(),
                "/dev/vg_kcore/k8s-local".to_string()
            ]
        );
    }

    #[test]
    fn grow_mounted_xfs_uses_the_mount_point() {
        let (commands, _) = resize_commands(
            "/dev/vg_kcore/k8s-local",
            2 * MIB,
            false,
            &facts(MIB, Some("/var/lib/kcore/k8s"), Some("xfs")),
        )
        .unwrap();
        assert_eq!(
            commands.last().unwrap(),
            &vec!["xfs_growfs".to_string(), "/var/lib/kcore/k8s".to_string()]
        );
    }

    #[test]
    fn shrink_refuses_a_mounted_or_xfs_volume() {
        let mounted = resize_commands(
            "/dev/vg_kcore/k8s-local",
            MIB,
            true,
            &facts(2 * MIB, Some("/var/lib/kcore/k8s"), Some("ext4")),
        )
        .unwrap_err();
        assert_eq!(mounted.kind, Kind::FailedPrecondition);
        let xfs = resize_commands(
            "/dev/vg_kcore/k8s-local",
            MIB,
            true,
            &facts(2 * MIB, None, Some("xfs")),
        )
        .unwrap_err();
        assert!(xfs.message.contains("xfs"));
        let no_flag = resize_commands(
            "/dev/vg_kcore/k8s-local",
            MIB,
            false,
            &facts(2 * MIB, None, None),
        )
        .unwrap_err();
        assert!(no_flag.message.contains("allow_shrink"));
    }

    #[test]
    fn shrink_ext4_shrinks_the_filesystem_first() {
        let (commands, _) = resize_commands(
            "/dev/vg_kcore/k8s-local",
            MIB,
            true,
            &facts(2 * MIB, None, Some("ext4")),
        )
        .unwrap();
        assert_eq!(commands[0][0], "e2fsck");
        assert_eq!(commands[1][0], "resize2fs");
        assert_eq!(commands[2][0], "lvreduce");
    }

    #[test]
    fn format_refuses_a_mounted_or_existing_filesystem() {
        let mounted = format_commands(
            "/dev/vg_kcore/k8s-local",
            "ext4",
            true,
            &facts(MIB, Some("/var/lib/kcore/k8s"), Some("ext4")),
        )
        .unwrap_err();
        assert!(mounted.message.contains("unmount"));
        let existing = format_commands(
            "/dev/vg_kcore/k8s-local",
            "ext4",
            false,
            &facts(MIB, None, Some("ext4")),
        )
        .unwrap_err();
        assert!(existing.message.contains("force"));
        let cmd = format_commands(
            "/dev/vg_kcore/k8s-local",
            "ext4",
            true,
            &facts(MIB, None, Some("ext4")),
        )
        .unwrap();
        assert_eq!(
            cmd,
            vec![
                "mkfs.ext4".to_string(),
                "-F".to_string(),
                "/dev/vg_kcore/k8s-local".to_string()
            ]
        );
    }

    #[test]
    fn mount_fs_type_uses_the_volume_or_the_request() {
        assert_eq!(mount_fs_type("", Some("ext4")).unwrap(), "ext4");
        assert!(mount_fs_type("", None).is_err());
        assert!(mount_fs_type("xfs", Some("ext4")).is_err());
        assert_eq!(mount_fs_type("XFS", None).unwrap(), "xfs");
    }

    #[test]
    fn mount_point_stays_under_the_data_directories() {
        assert_eq!(
            validate_mount_point("/var/lib/kcore/k8s").unwrap(),
            "/var/lib/kcore/k8s"
        );
        assert_eq!(
            validate_mount_point("/mnt/kcore/data").unwrap(),
            "/mnt/kcore/data"
        );
        assert!(validate_mount_point("/var/lib/kcore").is_err());
        assert!(validate_mount_point("/etc/kcore").is_err());
        assert!(validate_mount_point("/var/lib/kcore/../../etc").is_err());
    }

    #[test]
    fn mount_unit_name_escapes_hyphens() {
        assert_eq!(
            mount_unit_name("/var/lib/kcore/k8s"),
            "var-lib-kcore-k8s.mount"
        );
        assert_eq!(
            mount_unit_name("/var/lib/kcore/k8s-local"),
            "var-lib-kcore-k8s\\x2dlocal.mount"
        );
        let unit = mount_unit_contents("/dev/vg_kcore/k8s-local", "/var/lib/kcore/k8s", "ext4");
        assert!(unit.contains("What=/dev/vg_kcore/k8s-local\n"));
        assert!(unit.contains("Where=/var/lib/kcore/k8s\n"));
        assert!(unit.contains("Type=ext4\n"));
    }
}
