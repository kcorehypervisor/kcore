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

/// rbd-nbd runs librbd without its cache by default, so guests that issue small
/// sequential reads under O_DIRECT (firmware/GRUB loading a kernel) crawl at
/// ~10 MB/s. The writeback cache stays write-through until the guest first
/// flushes, and rbd-nbd flushes it on unmap.
pub const MAP_TUNING_WRITEBACK: [&str; 6] = [
    "--rbd-cache=true",
    "--rbd-cache-policy=writeback",
    "--rbd-readahead-trigger-requests=1",
    "--rbd-readahead-max-bytes=4194304",
    "--rbd-readahead-disable-after-bytes=0",
    "--rbd-default-map-options=rbd_blocklist_on_break_lock=true",
];

/// Prefer for failover-eligible guests: acknowledged writes reach the OSD
/// before the guest sees success.
pub const MAP_TUNING_WRITETHROUGH: [&str; 6] = [
    "--rbd-cache=true",
    "--rbd-cache-policy=writethrough",
    "--rbd-readahead-trigger-requests=1",
    "--rbd-readahead-max-bytes=4194304",
    "--rbd-readahead-disable-after-bytes=0",
    "--rbd-default-map-options=rbd_blocklist_on_break_lock=true",
];

/// Default map tuning (writeback). Prefer [`map_with_policy`].
pub const MAP_TUNING: [&str; 6] = MAP_TUNING_WRITEBACK;

pub fn map_tuning(cache_policy: &str) -> &'static [&'static str] {
    if cache_policy.eq_ignore_ascii_case("writethrough") {
        &MAP_TUNING_WRITETHROUGH
    } else {
        &MAP_TUNING_WRITEBACK
    }
}

/// Keyring `EnsureCephClientKey` writes for `client.kcore` on each member.
pub const CLIENT_KCORE_KEYRING: &str = "/etc/ceph/ceph.client.kcore.keyring";

/// `-n client.kcore` once that keyring is on this host, so day-to-day rbd
/// commands run with the pool-scoped key instead of `client.admin`. Empty
/// otherwise: callers keep using the admin key until the keyring arrives.
pub fn client_args() -> &'static [&'static str] {
    client_args_for(Path::new(CLIENT_KCORE_KEYRING))
}

pub fn client_args_for(keyring: &Path) -> &'static [&'static str] {
    if keyring.is_file() {
        &["-n", "client.kcore"]
    } else {
        &[]
    }
}

pub fn device_link(pool: &str, image: &str) -> PathBuf {
    Path::new(RBD_DEV_ROOT).join(pool).join(image)
}

/// Whether the kernel `rbd` module can map this cluster's keys.
///
/// Ceph 20 issues `aes256k` (32-byte) secrets. The in-tree libceph rejects them
/// (`secret too big 32`), so krbd is unusable against a current cluster even
/// when `/sys/module/rbd` exists. rbd-nbd stays the default; this helper only
/// reports the probe so operators and the truth doc share one answer.
pub fn krbd_usable() -> bool {
    krbd_usable_with(
        Path::new("/sys/module/rbd"),
        Path::new("/etc/ceph/ceph.client.admin.keyring"),
        Path::new("/etc/ceph/ceph.client.kcore.keyring"),
    )
}

pub fn krbd_usable_with(module: &Path, admin_keyring: &Path, kcore_keyring: &Path) -> bool {
    if !module.exists() {
        return false;
    }
    for path in [admin_keyring, kcore_keyring] {
        if let Ok(body) = std::fs::read_to_string(path) {
            if keyring_has_aes256k(&body) {
                return false;
            }
        }
    }
    true
}

fn keyring_has_aes256k(body: &str) -> bool {
    body.lines().any(|line| {
        let line = line.trim();
        line.starts_with("key ")
            && line
                .split_whitespace()
                .nth(2)
                .map(|k| {
                    // Base64 of 32 bytes is 44 chars with padding, or 43 without.
                    let bare = k.trim_end_matches('=');
                    bare.len() >= 42
                })
                .unwrap_or(false)
    })
}

/// Extract the `/dev/nbdN` node for `pool/image` from `rbd device list -t nbd --format json`.
pub fn parse_mapped_device(json: &str, pool: &str, image: &str) -> Option<String> {
    parse_mapped_device_snap(json, pool, image, None)
}

/// Like [`parse_mapped_device`], but match a specific snapshot name.
/// `snap = None` matches the image head (`snap` absent, `"-"`, or `""`).
pub fn parse_mapped_device_snap(
    json: &str,
    pool: &str,
    image: &str,
    snap: Option<&str>,
) -> Option<String> {
    let entries: serde_json::Value = serde_json::from_str(json).ok()?;
    entries.as_array()?.iter().find_map(|e| {
        let same_image = e.get("pool")?.as_str()? == pool && e.get("image")?.as_str()? == image;
        let entry_snap = e.get("snap").and_then(|s| s.as_str());
        let snap_ok = match snap {
            None => matches!(entry_snap, None | Some("-" | "")),
            Some(want) => entry_snap == Some(want),
        };
        (same_image && snap_ok)
            .then(|| e.get("device").and_then(|d| d.as_str()).map(str::to_string))
            .flatten()
    })
}

/// The nbd node currently serving `pool/image@snap`, if mapped.
pub fn mapped_device_snap(pool: &str, image: &str, snap: &str) -> Option<String> {
    parse_mapped_device_snap(&list_nbd_json(), pool, image, Some(snap))
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
    map_with_policy(pool, image, &env_cache_policy())
}

/// `KCORE_RBD_CACHE_POLICY` (`writeback` or `writethrough`); empty when unset.
pub fn env_cache_policy() -> String {
    std::env::var("KCORE_RBD_CACHE_POLICY").unwrap_or_default()
}

/// Like [`map`], with an explicit cache policy (`writeback` or `writethrough`).
pub fn map_with_policy(pool: &str, image: &str, cache_policy: &str) -> Result<PathBuf, String> {
    if let Some(dev) = mapped_device(pool, image) {
        point_link_at(pool, image, &dev)?;
        return Ok(device_link(pool, image));
    }
    let _ = Command::new("modprobe").arg("nbd").status();
    let spec = format!("{pool}/{image}");
    let run_map = |client: &[&str]| {
        Command::new("systemd-run")
            .args(["--scope", "--quiet", "--collect", "--", "rbd"])
            .args(client)
            .args(["device", "map", "-t", "nbd"])
            .args(map_tuning(cache_policy))
            .arg(&spec)
            .output()
            .map_err(|e| format!("rbd-nbd map: {e}"))
    };
    let client = client_args();
    let mut out = run_map(client)?;
    if !out.status.success() && !client.is_empty() {
        // Caps on a stale client.kcore key must not stop a guest from booting.
        out = run_map(&[])?;
    }
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

/// True when a device-mapper target (LUKS) is stacked on `dev` (`/dev/nbdN`).
fn has_holders(dev: &str) -> bool {
    let Some(name) = Path::new(dev).file_name() else {
        return false;
    };
    std::fs::read_dir(Path::new("/sys/block").join(name).join("holders"))
        .map(|mut d| d.next().is_some())
        .unwrap_or(false)
}

/// Close the `kcore-vol-*` LUKS mappers stacked on `dev`. `cryptsetup close`
/// refuses a mapper that is still open, so a running guest keeps its disk.
fn close_holder_mappers(dev: &str) -> Result<(), String> {
    let Some(name) = Path::new(dev).file_name() else {
        return Ok(());
    };
    let Ok(holders) = std::fs::read_dir(Path::new("/sys/block").join(name).join("holders")) else {
        return Ok(());
    };
    for holder in holders.flatten() {
        let Ok(dm_name) = std::fs::read_to_string(holder.path().join("dm/name")) else {
            continue;
        };
        let dm_name = dm_name.trim();
        if dm_name.starts_with("kcore-vol-") {
            crate::volume_crypto::close_mapper_only(dm_name)?;
        }
    }
    Ok(())
}

/// True when any process holds `dev` (a `/dev/nbdN` path) open, or a
/// device-mapper target is stacked on it.
pub fn device_in_use(dev: &str) -> bool {
    if has_holders(dev) {
        return true;
    }
    let Ok(procs) = std::fs::read_dir("/proc") else {
        return false;
    };
    for p in procs.flatten() {
        let name = p.file_name();
        if !name.to_string_lossy().bytes().all(|b| b.is_ascii_digit()) {
            continue;
        }
        // The rbd-nbd daemon keeps its own device node open for as long as it runs.
        if std::fs::read_to_string(p.path().join("comm")).is_ok_and(|c| c.trim() == "rbd-nbd") {
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

/// Unmap a mapped `/dev/nbdN` regardless of which image it carries and drop the
/// `/dev/rbd/<pool>/<image>` links that point at it.
pub fn unmap_device(dev: &str) -> Result<(), String> {
    close_holder_mappers(dev)?;
    if device_in_use(dev) {
        return Err(format!("rbd device {dev} is still in use"));
    }
    let _ = Command::new("blockdev").args(["--flushbufs", dev]).status();
    let out = Command::new("rbd")
        .args(client_args())
        .args(["device", "unmap", "-t", "nbd", dev])
        .output()
        .map_err(|e| format!("rbd-nbd unmap: {e}"))?;
    if !out.status.success() {
        return Err(format!(
            "rbd-nbd unmap {dev} failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    if let Ok(pools) = std::fs::read_dir(RBD_DEV_ROOT) {
        for pool in pools.flatten() {
            let Ok(images) = std::fs::read_dir(pool.path()) else {
                continue;
            };
            for image in images.flatten() {
                if std::fs::read_link(image.path()).is_ok_and(|t| t == Path::new(dev)) {
                    let _ = std::fs::remove_file(image.path());
                }
            }
        }
    }
    Ok(())
}

/// Unmap `pool/image` (idempotent). Fails while a process still has the device open.
pub fn unmap(pool: &str, image: &str) -> Result<(), String> {
    let link = device_link(pool, image);
    let Some(dev) = mapped_device(pool, image) else {
        let _ = std::fs::remove_file(&link);
        return Ok(());
    };
    close_holder_mappers(&dev)?;
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
        .args(client_args())
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
    fn finds_snapshot_device() {
        assert_eq!(
            parse_mapped_device_snap(LIST, "kcore-vms", "kcore-vm-b", Some("s1")).as_deref(),
            Some("/dev/nbd2")
        );
        assert_eq!(
            parse_mapped_device_snap(LIST, "kcore-vms", "kcore-vm-b", None).as_deref(),
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

    #[test]
    fn client_args_follow_the_keyring_file() {
        let dir = std::env::temp_dir().join(format!("kcore-rbd-client-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let kr = dir.join("ceph.client.kcore.keyring");
        let _ = std::fs::remove_file(&kr);
        assert!(client_args_for(&kr).is_empty());
        std::fs::write(&kr, "[client.kcore]\n\tkey = x\n").unwrap();
        assert_eq!(client_args_for(&kr), ["-n", "client.kcore"]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn map_tuning_selects_policy() {
        assert!(map_tuning("writethrough").contains(&"--rbd-cache-policy=writethrough"));
        assert!(map_tuning("WriteThrough").contains(&"--rbd-cache-policy=writethrough"));
        assert!(map_tuning("writeback").contains(&"--rbd-cache-policy=writeback"));
        assert!(map_tuning("").contains(&"--rbd-cache-policy=writeback"));
    }

    #[test]
    fn krbd_refuses_aes256k_keyrings() {
        let dir = std::env::temp_dir().join(format!("kcore-krbd-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let module = dir.join("module");
        std::fs::create_dir_all(&module).unwrap();
        let admin = dir.join("admin.keyring");
        let kcore = dir.join("kcore.keyring");
        // 32-byte secret base64 (aes256k).
        std::fs::write(
            &admin,
            "[client.admin]\n\tkey = AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=\n",
        )
        .unwrap();
        std::fs::write(&kcore, "").unwrap();
        assert!(!krbd_usable_with(&module, &admin, &kcore));
        std::fs::write(&admin, "[client.admin]\n\tkey = AQShort==\n").unwrap();
        assert!(krbd_usable_with(&module, &admin, &kcore));
        assert!(!krbd_usable_with(
            Path::new("/no/such/module"),
            &admin,
            &kcore
        ));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
