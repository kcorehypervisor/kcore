//! Host zvol operations for the node storage API.
//!
//! VM disks keep the configured dataset prefix. An explicit name creates a
//! zvol in the same pool without that prefix. Format, mount, and resize then
//! manage the `/dev/zvol` device. Command plans are pure so tests do not need ZFS.

use crate::lvm_ops::{self, BlockFacts, Kind, LvmOpError};

const MIB: i64 = 1024 * 1024;

/// `pool/name` passed to `zfs create`. An empty name keeps `<prefix><volume_id>`.
pub fn dataset_name(
    pool: &str,
    prefix: &str,
    volume_id: &str,
    explicit: &str,
) -> Result<String, LvmOpError> {
    let pool = lvm_ops::validate_component(pool.trim(), "pool")?;
    let explicit = explicit.trim();
    let name = if explicit.is_empty() {
        let id = lvm_ops::sanitize_volume_id(volume_id);
        lvm_ops::validate_component(&format!("{prefix}{id}"), "dataset")?
    } else {
        lvm_ops::validate_component(explicit, "dataset")?
    };
    Ok(format!("{pool}/{name}"))
}

/// `(dataset, /dev/zvol/dataset)` when the handle is in `pool`.
pub fn zvol_location(pool: &str, backend_handle: &str) -> Result<(String, String), LvmOpError> {
    let pool = lvm_ops::validate_component(pool.trim(), "pool")?;
    let handle = backend_handle.trim();
    let dataset = handle.strip_prefix("/dev/zvol/").unwrap_or(handle);
    let prefix = format!("{pool}/");
    let name = dataset.strip_prefix(&prefix).ok_or_else(|| {
        LvmOpError::new(
            Kind::InvalidArgument,
            format!("zvol must be under {prefix}"),
        )
    })?;
    let name = lvm_ops::validate_component(name, "dataset")?;
    let dataset = format!("{pool}/{name}");
    Ok((dataset.clone(), format!("/dev/zvol/{dataset}")))
}

pub fn resize_commands(
    dataset: &str,
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
            "shrinking a zvol requires allow_shrink",
        ));
    }
    if shrinking && facts.mounted_at.is_some() {
        return Err(LvmOpError::new(
            Kind::FailedPrecondition,
            "unmount the zvol before shrinking it",
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
        commands.push(set_volsize(dataset, &size));
        return Ok((commands, format!("shrunk {device} to {size_bytes} bytes")));
    }
    if shrinking {
        commands.push(set_volsize(dataset, &size));
        return Ok((commands, format!("shrunk {device} to {size_bytes} bytes")));
    }

    commands.push(set_volsize(dataset, &size));
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

pub fn resize_volume(
    pool: &str,
    backend_handle: &str,
    size_bytes: i64,
    allow_shrink: bool,
) -> Result<String, LvmOpError> {
    let (dataset, device) = zvol_location(pool, backend_handle)?;
    let (mounted_at, fs_type) = lvm_ops::block_fs(&device)?;
    let facts = BlockFacts {
        size_bytes: volsize(&dataset)?,
        mounted_at,
        fs_type,
    };
    let (commands, message) = resize_commands(&dataset, &device, size_bytes, allow_shrink, &facts)?;
    lvm_ops::run_plan(&commands)?;
    Ok(message)
}

pub fn format_volume(
    pool: &str,
    backend_handle: &str,
    fs_type: &str,
    force: bool,
) -> Result<String, LvmOpError> {
    let (_dataset, device) = zvol_location(pool, backend_handle)?;
    lvm_ops::format_device(&device, fs_type, force)
}

pub fn mount_volume(
    pool: &str,
    backend_handle: &str,
    mount_point: &str,
    fs_type: &str,
    persistent: bool,
) -> Result<String, LvmOpError> {
    let (_dataset, device) = zvol_location(pool, backend_handle)?;
    lvm_ops::mount_device(&device, mount_point, fs_type, persistent)
}

fn set_volsize(dataset: &str, size: &str) -> Vec<String> {
    vec![
        "zfs".into(),
        "set".into(),
        format!("volsize={size}"),
        dataset.into(),
    ]
}

fn volsize(dataset: &str) -> Result<i64, LvmOpError> {
    let stdout = lvm_ops::capture(
        "zfs",
        &["get", "-H", "-p", "-o", "value", "volsize", dataset],
    )?;
    let token = stdout.split_whitespace().next().unwrap_or("");
    token.parse::<i64>().map_err(|_| {
        LvmOpError::new(
            Kind::Internal,
            format!("could not parse zvol size from {stdout:?}"),
        )
    })
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
        let dataset = dataset_name("tank0", "kcore-", "ignored", "k8s-local").unwrap();
        assert_eq!(dataset, "tank0/k8s-local");
    }

    #[test]
    fn empty_name_keeps_the_vm_prefix() {
        let dataset = dataset_name("tank0", "kcore-", "win 01", "").unwrap();
        assert_eq!(dataset, "tank0/kcore-win-01");
    }

    #[test]
    fn name_rejects_path_pieces() {
        assert!(dataset_name("tank0", "kcore-", "vm", "../root").is_err());
        assert!(dataset_name("tank0", "kcore-", "vm", "nested/name").is_err());
        assert!(dataset_name("tank0/child", "kcore-", "vm", "data").is_err());
    }

    #[test]
    fn device_must_stay_in_the_pool() {
        let (dataset, device) = zvol_location("tank0", "/dev/zvol/tank0/k8s-local").unwrap();
        assert_eq!(dataset, "tank0/k8s-local");
        assert_eq!(device, "/dev/zvol/tank0/k8s-local");
        assert!(zvol_location("tank0", "/dev/zvol/other/k8s-local").is_err());
        assert!(zvol_location("tank0", "/dev/vg_kcore/k8s-local").is_err());
        assert!(zvol_location("tank0", "tank0/../root").is_err());
    }

    #[test]
    fn grow_ext4_sets_volsize_then_the_filesystem() {
        let (commands, message) = resize_commands(
            "tank0/k8s-local",
            "/dev/zvol/tank0/k8s-local",
            2 * MIB,
            false,
            &facts(MIB, None, Some("ext4")),
        )
        .unwrap();
        assert!(message.contains("grew"));
        assert_eq!(commands[0][0], "zfs");
        assert_eq!(commands[0][2], format!("volsize={}B", 2 * MIB));
        assert_eq!(commands[1][0], "resize2fs");
    }

    #[test]
    fn grow_mounted_xfs_uses_the_mount_point() {
        let (commands, _) = resize_commands(
            "tank0/k8s-local",
            "/dev/zvol/tank0/k8s-local",
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
    fn shrink_refuses_a_mounted_or_xfs_zvol() {
        let mounted = resize_commands(
            "tank0/k8s-local",
            "/dev/zvol/tank0/k8s-local",
            MIB,
            true,
            &facts(2 * MIB, Some("/var/lib/kcore/k8s"), Some("ext4")),
        )
        .unwrap_err();
        assert_eq!(mounted.kind, Kind::FailedPrecondition);
        let xfs = resize_commands(
            "tank0/k8s-local",
            "/dev/zvol/tank0/k8s-local",
            MIB,
            true,
            &facts(2 * MIB, None, Some("xfs")),
        )
        .unwrap_err();
        assert!(xfs.message.contains("xfs"));
    }

    #[test]
    fn shrink_ext4_shrinks_the_filesystem_first() {
        let (commands, _) = resize_commands(
            "tank0/k8s-local",
            "/dev/zvol/tank0/k8s-local",
            MIB,
            true,
            &facts(2 * MIB, None, Some("ext4")),
        )
        .unwrap();
        assert_eq!(commands[0][0], "e2fsck");
        assert_eq!(commands[1][0], "resize2fs");
        assert_eq!(commands[2][0], "zfs");
        assert!(commands[2][2].starts_with("volsize="));
    }
}
