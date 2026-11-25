//! Planning for `kctl volume convert` and `kctl volume copy`.
//!
//! The controller streams extents between node agents; this module decides
//! whether a request is allowed and owns the arithmetic and names. Nothing
//! here talks to a node.

use serde_json::{json, Map, Value};

/// Bytes read from the source per `ReadVolumeExtents` call. The node reads a
/// whole request into memory before streaming it, so this bounds its memory.
pub const COPY_WINDOW_BYTES: i64 = 64 * 1024 * 1024;

/// Transfer block size. Under tonic's 4 MiB default message limit, so the
/// controller's node clients need no larger limit.
pub const COPY_BLOCK_BYTES: i32 = 1024 * 1024;

/// Lease lifetime on the source node. Released explicitly when the copy ends.
pub const COPY_LEASE_SECONDS: i32 = 86_400;

/// Backends a VM can be converted to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConvertTarget {
    Ceph,
}

pub fn parse_target(to: &str) -> Result<ConvertTarget, String> {
    match to.trim().to_ascii_lowercase().as_str() {
        "ceph" => Ok(ConvertTarget::Ceph),
        "lvm" | "zfs" => Err(format!(
            "converting to {} is not implemented; only --to ceph is",
            to.trim()
        )),
        "" => Err("--to is required (ceph)".into()),
        other => Err(format!("unknown backend '{other}'; only ceph is supported")),
    }
}

/// What the controller will do for a convert request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ConvertPlan {
    /// The guest is running and will be stopped for the copy.
    pub stop_guest: bool,
}

/// Refuse a convert that cannot work, before anything is stopped.
pub fn plan_convert(
    backend: &str,
    running: bool,
    allow_stop: bool,
    node_is_ceph_member: bool,
    node_id: &str,
) -> Result<ConvertPlan, String> {
    match backend {
        "ceph" => return Err("VM is already on ceph".into()),
        "lvm" | "zfs" => {}
        other => {
            return Err(format!(
                "VMs on the {other} backend cannot be converted; only lvm and zfs disks can"
            ))
        }
    }
    if !node_is_ceph_member {
        return Err(format!(
            "node {node_id} is not a healthy CephCluster member, so a Ceph VM could not run \
             there; join the node to a CephCluster first"
        ));
    }
    if running && !allow_stop {
        return Err(
            "the guest is running and must be stopped for the copy; pass --yes to allow it".into(),
        );
    }
    Ok(ConvertPlan {
        stop_guest: running,
    })
}

/// `(offset, length)` windows covering `[0, total)`.
pub fn copy_windows(total: i64, window: i64) -> Vec<(i64, i64)> {
    if total <= 0 || window <= 0 {
        return Vec::new();
    }
    let mut out = Vec::with_capacity(((total + window - 1) / window) as usize);
    let mut offset = 0;
    while offset < total {
        let len = window.min(total - offset);
        out.push((offset, len));
        offset += len;
    }
    out
}

/// RBD sizes are whole MiB; a zvol need not be.
pub fn rbd_image_bytes(source_bytes: i64) -> i64 {
    const MIB: i64 = 1024 * 1024;
    if source_bytes <= 0 {
        return 0;
    }
    (source_bytes + MIB - 1) / MIB * MIB
}

/// The destination reports data and zero runs alike in `bytes_written`, so a
/// copy that covered every window writes exactly the source size.
pub fn copy_complete(source_bytes: i64, bytes_written: i64) -> bool {
    source_bytes > 0 && bytes_written == source_bytes
}

pub fn progress_detail(copied: i64, total: i64) -> String {
    json!({ "copy": { "bytesCopied": copied, "bytesTotal": total } }).to_string()
}

/// Cluster that holds `source_json`'s volume. An explicit name wins, then the
/// stamp left by an earlier convert or copy, then the only cluster there is.
pub fn resolve_source_cluster(
    explicit: &str,
    source_json: &str,
    clusters: &[String],
) -> Result<String, String> {
    let explicit = explicit.trim();
    if !explicit.is_empty() {
        return if clusters.iter().any(|c| c == explicit) {
            Ok(explicit.to_string())
        } else {
            Err(format!("CephCluster '{explicit}' not found"))
        };
    }
    if let Some(stamped) = stamped_cluster(source_json) {
        if clusters.contains(&stamped) {
            return Ok(stamped);
        }
        return Err(format!(
            "volume was recorded on CephCluster '{stamped}', which no longer exists; \
             pass --from-cluster"
        ));
    }
    match clusters {
        [only] => Ok(only.clone()),
        [] => Err("no CephCluster is registered".into()),
        _ => Err(
            "more than one CephCluster exists and the volume does not record which one \
             holds it; pass --from-cluster"
                .into(),
        ),
    }
}

pub fn stamped_cluster(source_json: &str) -> Option<String> {
    let v: Value = serde_json::from_str(source_json).ok()?;
    v.get("cephCluster")?
        .as_str()
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

/// `source_json` with the cluster (and, for a copy, its origin) recorded.
/// Existing keys survive; a value that is not a JSON object is replaced.
pub fn stamp_source_json(existing: &str, cluster: &str, copied_from: Option<&str>) -> String {
    let mut map = match serde_json::from_str::<Value>(existing) {
        Ok(Value::Object(m)) => m,
        _ => Map::new(),
    };
    map.insert("cephCluster".into(), json!(cluster));
    if let Some(src) = copied_from {
        map.insert("copiedFrom".into(), json!(src));
    }
    Value::Object(map).to_string()
}

/// Cluster whose spec lists `node_id` as a member.
pub fn cluster_for_node(clusters: &[(String, Vec<String>)], node_id: &str) -> Option<String> {
    clusters
        .iter()
        .find(|(_, members)| members.iter().any(|m| m == node_id))
        .map(|(name, _)| name.clone())
}

/// Name for the copy: the caller's, else `<source>-copy`.
pub fn copy_target_name(source: &str, requested: &str) -> Result<String, String> {
    let name = if requested.trim().is_empty() {
        format!("{}-copy", source.trim())
    } else {
        requested.trim().to_string()
    };
    if name.is_empty() || name.len() > 63 {
        return Err("volume name must be 1 to 63 characters".into());
    }
    if !name
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
    {
        return Err("volume name may use only letters, digits, '-', '_' and '.'".into());
    }
    Ok(name)
}

/// Refuse a copy that has nothing to copy between.
pub fn plan_copy(source_cluster: &str, dest_cluster: &str) -> Result<(), String> {
    if source_cluster == dest_cluster {
        return Err(format!(
            "volume is already on CephCluster '{dest_cluster}'; use a snapshot clone to copy \
             within one cluster"
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn target_accepts_only_ceph() {
        assert_eq!(parse_target(" Ceph ").unwrap(), ConvertTarget::Ceph);
        assert!(parse_target("lvm").unwrap_err().contains("not implemented"));
        assert!(parse_target("zfs").is_err());
        assert!(parse_target("").unwrap_err().contains("required"));
        assert!(parse_target("nfs").unwrap_err().contains("unknown"));
    }

    #[test]
    fn convert_refuses_what_cannot_work() {
        assert!(plan_convert("ceph", false, true, true, "n1")
            .unwrap_err()
            .contains("already"));
        assert!(plan_convert("filesystem", false, true, true, "n1").is_err());
        let e = plan_convert("lvm", false, true, false, "node-9").unwrap_err();
        assert!(e.contains("node-9"), "{e}");
    }

    #[test]
    fn convert_needs_consent_to_stop_a_running_guest() {
        let e = plan_convert("zfs", true, false, true, "n1").unwrap_err();
        assert!(e.contains("--yes"), "{e}");
        assert_eq!(
            plan_convert("zfs", true, true, true, "n1").unwrap(),
            ConvertPlan { stop_guest: true }
        );
        assert_eq!(
            plan_convert("lvm", false, false, true, "n1").unwrap(),
            ConvertPlan { stop_guest: false }
        );
    }

    #[test]
    fn windows_cover_the_disk_exactly() {
        assert!(copy_windows(0, 10).is_empty());
        assert!(copy_windows(10, 0).is_empty());
        assert_eq!(copy_windows(10, 10), [(0, 10)]);
        assert_eq!(copy_windows(25, 10), [(0, 10), (10, 10), (20, 5)]);
        let w = copy_windows(3 * COPY_WINDOW_BYTES + 1, COPY_WINDOW_BYTES);
        assert_eq!(w.len(), 4);
        assert_eq!(
            w.iter().map(|(_, l)| l).sum::<i64>(),
            3 * COPY_WINDOW_BYTES + 1
        );
        for pair in w.windows(2) {
            assert_eq!(pair[0].0 + pair[0].1, pair[1].0);
        }
    }

    #[test]
    fn rbd_size_rounds_up_to_a_mib() {
        const MIB: i64 = 1024 * 1024;
        assert_eq!(rbd_image_bytes(0), 0);
        assert_eq!(rbd_image_bytes(-5), 0);
        assert_eq!(rbd_image_bytes(MIB), MIB);
        assert_eq!(rbd_image_bytes(MIB + 1), 2 * MIB);
        assert_eq!(rbd_image_bytes(10 * 1024 * MIB), 10 * 1024 * MIB);
    }

    #[test]
    fn a_short_write_is_not_a_finished_copy() {
        assert!(copy_complete(100, 100));
        assert!(!copy_complete(100, 99));
        assert!(!copy_complete(100, 101));
        assert!(!copy_complete(0, 0));
    }

    #[test]
    fn progress_is_json() {
        let v: Value = serde_json::from_str(&progress_detail(5, 10)).unwrap();
        assert_eq!(v["copy"]["bytesCopied"], 5);
        assert_eq!(v["copy"]["bytesTotal"], 10);
    }

    #[test]
    fn source_cluster_resolution_order() {
        let two = names(&["a", "b"]);
        assert_eq!(resolve_source_cluster("b", "{}", &two).unwrap(), "b");
        assert!(resolve_source_cluster("zzz", "{}", &two).is_err());
        assert_eq!(
            resolve_source_cluster("", r#"{"cephCluster":"a"}"#, &two).unwrap(),
            "a"
        );
        assert!(
            resolve_source_cluster("", r#"{"cephCluster":"gone"}"#, &two)
                .unwrap_err()
                .contains("--from-cluster")
        );
        assert_eq!(
            resolve_source_cluster("", "{}", &names(&["only"])).unwrap(),
            "only"
        );
        assert!(resolve_source_cluster("", "{}", &two)
            .unwrap_err()
            .contains("--from-cluster"));
        assert!(resolve_source_cluster("", "not json", &[]).is_err());
    }

    #[test]
    fn stamp_keeps_other_keys() {
        let out = stamp_source_json(r#"{"snapshotId":"s1"}"#, "dr", Some("vol-1"));
        let v: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["snapshotId"], "s1");
        assert_eq!(v["cephCluster"], "dr");
        assert_eq!(v["copiedFrom"], "vol-1");
        assert_eq!(stamped_cluster(&out).as_deref(), Some("dr"));
        let fresh = stamp_source_json("garbage", "dr", None);
        assert_eq!(stamped_cluster(&fresh).as_deref(), Some("dr"));
        assert!(!fresh.contains("copiedFrom"));
    }

    #[test]
    fn cluster_for_node_finds_the_member() {
        let clusters = vec![
            ("a".to_string(), names(&["n1", "n2"])),
            ("b".to_string(), names(&["n3"])),
        ];
        assert_eq!(cluster_for_node(&clusters, "n3").as_deref(), Some("b"));
        assert_eq!(cluster_for_node(&clusters, "n9"), None);
    }

    #[test]
    fn copy_name_defaults_and_validates() {
        assert_eq!(copy_target_name("db", "").unwrap(), "db-copy");
        assert_eq!(copy_target_name("db", " db2 ").unwrap(), "db2");
        assert!(copy_target_name("db", "a b").is_err());
        assert!(copy_target_name("db", &"x".repeat(64)).is_err());
        assert!(copy_target_name("db", "../x").is_err());
    }

    #[test]
    fn copy_needs_two_clusters() {
        assert!(plan_copy("a", "a").unwrap_err().contains("snapshot clone"));
        assert!(plan_copy("a", "b").is_ok());
    }
}
