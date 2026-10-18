//! Instant VM recovery: a local qcow2 overlay in front of a read-only NBD export.
//!
//! The coordinator exports the backup. This module builds the `qemu-img`
//! command that creates the overlay and reads `qemu-img map` output so
//! finalize can copy only the changed extents with `WriteVolumeBlocks`.

use std::path::Path;

/// Arguments for `qemu-img create -f qcow2 -b <nbd> -F raw <overlay>`.
pub fn cow_create_args(nbd_url: &str, overlay: &Path) -> Vec<String> {
    vec![
        "create".into(),
        "-f".into(),
        "qcow2".into(),
        "-b".into(),
        nbd_url.to_string(),
        "-F".into(),
        "raw".into(),
        overlay.display().to_string(),
    ]
}

/// Cloud Hypervisor opens the overlay. The NBD export stays the backing file.
pub fn disk_path(overlay: &Path) -> String {
    overlay.display().to_string()
}

/// `qemu-io` reads one guest-visible extent out of the overlay.
pub fn qemu_io_read_args(overlay: &Path, offset: u64, length: u64, out: &Path) -> Vec<String> {
    vec![
        "-f".into(),
        "qcow2".into(),
        "-r".into(),
        "-c".into(),
        format!("read -q {offset} {length} {}", out.display()),
        overlay.display().to_string(),
    ]
}

/// Cloud Hypervisor `vm.create` body. The disk serial is `vault-instant` so
/// finalize can swap it for the cluster volume.
pub fn vm_create_body(
    disk: &Path,
    memory_bytes: u64,
    vcpus: u32,
    firmware: &str,
) -> serde_json::Value {
    serde_json::json!({
        "cpus": { "boot_vcpus": vcpus, "max_vcpus": vcpus },
        "memory": { "size": memory_bytes },
        "payload": { "firmware": firmware },
        "disks": [{ "path": disk.display().to_string(), "serial": "vault-instant" }],
    })
}

pub fn prepare_overlay(nbd_url: &str, dir: &Path) -> Result<std::path::PathBuf, String> {
    std::fs::create_dir_all(dir).map_err(|e| format!("overlay dir: {e}"))?;
    let overlay = dir.join("overlay.qcow2");
    let args = cow_create_args(nbd_url, &overlay);
    let output = std::process::Command::new("qemu-img")
        .args(&args)
        .output()
        .map_err(|e| format!("qemu-img: {e}"))?;
    if !output.status.success() {
        return Err(format!(
            "qemu-img create: {}",
            String::from_utf8_lossy(&output.stderr)
        ));
    }
    Ok(overlay)
}

/// Copy depth-0 extents from the overlay onto `device` with the same writer
/// `WriteVolumeExtents` uses.
pub fn copy_overlay(overlay: &Path, device: &Path) -> Result<i64, String> {
    use crate::storage::export::{write_extent_unit, TransferUnit};
    let output = std::process::Command::new("qemu-img")
        .args(["map", "--output=json"])
        .arg(overlay)
        .output()
        .map_err(|e| format!("qemu-img map: {e}"))?;
    if !output.status.success() {
        return Err(format!(
            "qemu-img map: {}",
            String::from_utf8_lossy(&output.stderr)
        ));
    }
    let json = String::from_utf8(output.stdout).map_err(|e| e.to_string())?;
    let extents = overlay_extents(&json)?;
    let mut bytes = 0i64;
    for (start, length) in extents {
        let mut done = 0u64;
        while done < length {
            let n = (length - done).min(8 * 1024 * 1024);
            let tmp = std::env::temp_dir().join(format!("kcore-instant-{start}-{done}"));
            let args = qemu_io_read_args(overlay, start + done, n, &tmp);
            let read = std::process::Command::new("qemu-io")
                .args(&args)
                .output()
                .map_err(|e| format!("qemu-io: {e}"))?;
            if !read.status.success() {
                let _ = std::fs::remove_file(&tmp);
                return Err(format!(
                    "qemu-io: {}",
                    String::from_utf8_lossy(&read.stderr)
                ));
            }
            let data = std::fs::read(&tmp).map_err(|e| format!("read extent: {e}"))?;
            let _ = std::fs::remove_file(&tmp);
            let unit = TransferUnit::Data {
                offset: (start + done) as i64,
                length: data.len() as i64,
            };
            let (wrote, _, _) = write_extent_unit(device, &unit, Some(&data), false)
                .map_err(|e| format!("write extent: {e}"))?;
            bytes += wrote;
            done += n;
        }
    }
    Ok(bytes)
}

pub async fn boot_overlay(
    vm_name: &str,
    disk: &Path,
    memory_bytes: u64,
    vcpus: u32,
) -> Result<u32, String> {
    let socket_dir =
        std::env::var("KCORE_CH_SOCKET_DIR").unwrap_or_else(|_| "/run/kcore/ch".to_string());
    std::fs::create_dir_all(&socket_dir).map_err(|e| format!("socket dir: {e}"))?;
    let client = crate::vmm::Client::new(&socket_dir);
    let ch = crate::live_migrate::resolve_ch_bin();
    let pid = crate::live_migrate::spawn_receive_vmm(&client, vm_name, &ch).await?;
    let firmware = std::env::var("KCORE_CH_FIRMWARE")
        .unwrap_or_else(|_| "/usr/share/cloud-hypervisor/hypervisor-fw".to_string());
    let memory = if memory_bytes == 0 {
        512 * 1024 * 1024
    } else {
        memory_bytes
    };
    let vcpus = vcpus.max(1);
    let body = vm_create_body(disk, memory, vcpus, &firmware);
    if let Err(e) = client.create_and_boot(vm_name, &body).await {
        return Err(format!("boot {vm_name}: {e}"));
    }
    Ok(pid)
}

pub async fn swap_disk(vm_name: &str, volume_path: &str) -> Result<(), String> {
    let socket_dir =
        std::env::var("KCORE_CH_SOCKET_DIR").unwrap_or_else(|_| "/run/kcore/ch".to_string());
    let client = crate::vmm::Client::new(&socket_dir);
    client.remove_disk(vm_name, "vault-instant").await?;
    client
        .add_disk(vm_name, volume_path, "vault-volume", false)
        .await
}

/// Extents in the top image of a `qemu-img map --output=json` document.
/// Each pair is `(offset, length)` to send with `WriteVolumeBlocks`.
pub fn overlay_extents(map_json: &str) -> Result<Vec<(u64, u64)>, String> {
    let rows: Vec<serde_json::Value> =
        serde_json::from_str(map_json).map_err(|e| format!("qemu-img map: {e}"))?;
    let mut out = Vec::new();
    for row in rows {
        let depth = row.get("depth").and_then(|v| v.as_u64()).unwrap_or(1);
        let data = row.get("data").and_then(|v| v.as_bool()).unwrap_or(false);
        let zero = row.get("zero").and_then(|v| v.as_bool()).unwrap_or(false);
        if depth != 0 || !data || zero {
            continue;
        }
        let start = row
            .get("start")
            .and_then(|v| v.as_u64())
            .ok_or_else(|| "map extent missing start".to_string())?;
        let length = row
            .get("length")
            .and_then(|v| v.as_u64())
            .ok_or_else(|| "map extent missing length".to_string())?;
        if length > 0 {
            out.push((start, length));
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cow_command_points_the_overlay_at_the_nbd_export() {
        let args = cow_create_args("nbd://127.0.0.1:10809", Path::new("/run/kcore/cow.qcow2"));
        assert_eq!(
            args,
            vec![
                "create",
                "-f",
                "qcow2",
                "-b",
                "nbd://127.0.0.1:10809",
                "-F",
                "raw",
                "/run/kcore/cow.qcow2",
            ]
        );
        assert_eq!(
            disk_path(Path::new("/run/kcore/cow.qcow2")),
            "/run/kcore/cow.qcow2"
        );
    }

    #[test]
    fn vm_create_points_cloud_hypervisor_at_the_overlay() {
        let body = vm_create_body(
            Path::new("/run/kcore/instant/overlay.qcow2"),
            512 * 1024 * 1024,
            1,
            "/usr/share/cloud-hypervisor/hypervisor-fw",
        );
        assert_eq!(body["disks"][0]["path"], "/run/kcore/instant/overlay.qcow2");
        assert_eq!(body["disks"][0]["serial"], "vault-instant");
        assert_eq!(body["memory"]["size"], 512 * 1024 * 1024);
        let args = qemu_io_read_args(
            Path::new("/run/kcore/instant/overlay.qcow2"),
            4096,
            8192,
            Path::new("/tmp/extent"),
        );
        assert!(args.iter().any(|a| a.contains("read -q 4096 8192")));
    }

    #[test]
    fn finalize_keeps_only_top_image_data() {
        let map = r#"[
          {"start":0,"length":65536,"depth":1,"data":true,"zero":false},
          {"start":65536,"length":4096,"depth":0,"data":true,"zero":false},
          {"start":69632,"length":4096,"depth":0,"data":false,"zero":true}
        ]"#;
        let extents = overlay_extents(map).unwrap();
        assert_eq!(extents, vec![(65536, 4096)]);
    }
}
