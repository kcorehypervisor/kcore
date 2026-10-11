//! RBD mapping through `rbd-nbd` (librbd in userspace).
//!
//! Ceph 20 hands out `aes256k` cephx keys (32-byte secrets). The kernel
//! libceph client rejects them (`secret too big 32`), so `rbd map` cannot work
//! against a current cluster. Mapping with `rbd-nbd` keeps the stronger cipher
//! and still yields a plain block device. Every consumer (Cloud Hypervisor,
//! `qemu-img`, `cryptsetup`) keeps opening the stable path
//! `/dev/rbd/<pool>/<image>`, which is a symlink to the `/dev/nbdN` node.
//!
//! The mapping daemon is started inside its own transient scope so it outlives
//! both this agent and the VM unit that asked for the mapping; it is torn down
//! only by an explicit [`unmap`].

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

const RBD_DEV_ROOT: &str = "/dev/rbd";

pub fn device_link(pool: &str, image: &str) -> PathBuf {
    Path::new(RBD_DEV_ROOT).join(pool).join(image)
}

/// Extract the `/dev/nbdN` node for `pool/image` from `rbd device list -t nbd --format json`.
pub fn parse_mapped_device(json: &str, pool: &str, image: &str) -> Option<String> {
    let entries: serde_json::Value = serde_json::from_str(json).ok()?;
    entries.as_array()?.iter().find_map(|e| {
        let same_image = e.get("pool")?.as_str()? == pool && e.get("image")?.as_str()? == image;
        let head = matches!(
            e.get("snap").and_then(|s| s.as_str()),
            None | Some("-" | "")
        );
        (same_image && head)
            .then(|| e.get("device").and_then(|d| d.as_str()).map(str::to_string))
            .flatten()
    })
}

fn list_nbd_json() -> String {
    Command::new("rbd")
        .args(["device", "list", "-t", "nbd", "--format", "json"])
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
        .unwrap_or_default()
}

/// The nbd node currently serving `pool/image`, if a live mapping exists.
pub fn mapped_device(pool: &str, image: &str) -> Option<String> {
    parse_mapped_device(&list_nbd_json(), pool, image)
}

fn point_link_at(pool: &str, image: &str, dev: &str) -> Result<(), String> {
    let link = device_link(pool, image);
    if let Some(parent) = link.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("mkdir {}: {e}", parent.display()))?;
    }
    if std::fs::read_link(&link).ok().as_deref() == Some(Path::new(dev)) {
        return Ok(());
    }
    let _ = std::fs::remove_file(&link);
    std::os::unix::fs::symlink(dev, &link)
        .map_err(|e| format!("symlink {} -> {dev}: {e}", link.display()))
}

/// Map `pool/image` (idempotent) and return the stable `/dev/rbd/<pool>/<image>` path.
pub fn map(pool: &str, image: &str) -> Result<PathBuf, String> {
    if let Some(dev) = mapped_device(pool, image) {
        point_link_at(pool, image, &dev)?;
        return Ok(device_link(pool, image));
    }
    let _ = Command::new("modprobe").arg("nbd").status();
    let spec = format!("{pool}/{image}");
    let out = Command::new("systemd-run")
        .args([
            "--scope",
            "--quiet",
            "--collect",
            "--",
            "rbd",
            "device",
            "map",
            "-t",
            "nbd",
            &spec,
        ])
        .output()
        .map_err(|e| format!("rbd-nbd map: {e}"))?;
    if !out.status.success() {
        return Err(format!(
            "rbd-nbd map failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    let dev = String::from_utf8_lossy(&out.stdout)
        .lines()
        .rev()
        .find(|l| l.trim().starts_with("/dev/nbd"))
        .map(|l| l.trim().to_string())
        .or_else(|| mapped_device(pool, image))
        .ok_or_else(|| format!("rbd-nbd mapped {spec} but reported no device"))?;
    point_link_at(pool, image, &dev)?;
    for _ in 0..50 {
        if device_link(pool, image).exists() {
            return Ok(device_link(pool, image));
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    Err(format!(
        "{} did not appear after map",
        device_link(pool, image).display()
    ))
}

/// True when any process holds `dev` (a `/dev/nbdN` path) open.
pub fn device_in_use(dev: &str) -> bool {
    let Ok(procs) = std::fs::read_dir("/proc") else {
        return false;
    };
    for p in procs.flatten() {
        let name = p.file_name();
        if !name.to_string_lossy().bytes().all(|b| b.is_ascii_digit()) {
            continue;
        }
        let Ok(fds) = std::fs::read_dir(p.path().join("fd")) else {
            continue;
        };
        for fd in fds.flatten() {
            if std::fs::read_link(fd.path()).is_ok_and(|t| t == Path::new(dev)) {
                return true;
            }
        }
    }
    false
}

/// Unmap `pool/image` (idempotent). Fails while a process still has the device open.
pub fn unmap(pool: &str, image: &str) -> Result<(), String> {
    let link = device_link(pool, image);
    let Some(dev) = mapped_device(pool, image) else {
        let _ = std::fs::remove_file(&link);
        return Ok(());
    };
    if device_in_use(&dev) {
        return Err(format!(
            "rbd device {dev} for {pool}/{image} is still in use"
        ));
    }
    // Dirty pages left in the host page cache would be lost once the nbd
    // socket is torn down.
    let _ = Command::new("blockdev")
        .args(["--flushbufs", &dev])
        .status();
    let out = Command::new("rbd")
        .args(["device", "unmap", "-t", "nbd", &dev])
        .output()
        .map_err(|e| format!("rbd-nbd unmap: {e}"))?;
    if !out.status.success() && mapped_device(pool, image).is_some() {
        return Err(format!(
            "rbd-nbd unmap failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    let _ = std::fs::remove_file(&link);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const LIST: &str = r#"[
        {"id":"1","pool":"kcore-vms","namespace":"","image":"kcore-vm-a","snap":"-","device":"/dev/nbd0"},
        {"id":"2","pool":"kcore-vms","namespace":"","image":"kcore-vm-b","snap":"-","device":"/dev/nbd1"},
        {"id":"3","pool":"kcore-vms","namespace":"","image":"kcore-vm-b","snap":"s1","device":"/dev/nbd2"}
    ]"#;

    #[test]
    fn finds_head_image_device() {
        assert_eq!(
            parse_mapped_device(LIST, "kcore-vms", "kcore-vm-b").as_deref(),
            Some("/dev/nbd1")
        );
    }

    #[test]
    fn unknown_image_or_garbage_is_unmapped() {
        assert_eq!(parse_mapped_device(LIST, "kcore-vms", "nope"), None);
        assert_eq!(parse_mapped_device("", "kcore-vms", "kcore-vm-a"), None);
        assert_eq!(parse_mapped_device("{}", "kcore-vms", "kcore-vm-a"), None);
    }

    #[test]
    fn link_lives_under_dev_rbd() {
        assert_eq!(
            device_link("kcore-vms", "kcore-vm-a"),
            PathBuf::from("/dev/rbd/kcore-vms/kcore-vm-a")
        );
    }
}
