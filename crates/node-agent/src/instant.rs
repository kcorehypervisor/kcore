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
