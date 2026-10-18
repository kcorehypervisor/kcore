//! Controller-side bookkeeping for snapshot export leases and node selection.

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use crate::db::{NodeRow, VolumeRow, VolumeSnapshotRow};
use crate::host_volume;

#[derive(Debug, Clone)]
pub struct ExportLease {
    pub lease_id: String,
    pub node_id: String,
    pub node_address: String,
    pub volume_name: String,
    pub snapshot_name: String,
    pub device_path: String,
    pub size_bytes: i64,
    pub block_size: i32,
    pub expires_at: Instant,
}

#[derive(Debug, Default)]
pub struct ExportLeaseTable {
    inner: Mutex<HashMap<String, ExportLease>>,
}

impl ExportLeaseTable {
    pub fn insert(&self, lease: ExportLease) {
        if let Ok(mut g) = self.inner.lock() {
            g.insert(lease.lease_id.clone(), lease);
        }
    }

    pub fn get(&self, lease_id: &str) -> Option<ExportLease> {
        let mut g = self.inner.lock().ok()?;
        let lease = g.get(lease_id)?.clone();
        if Instant::now() >= lease.expires_at {
            g.remove(lease_id);
            return None;
        }
        Some(lease)
    }

    pub fn remove(&self, lease_id: &str) -> Option<ExportLease> {
        self.inner.lock().ok()?.remove(lease_id)
    }
}

pub fn global_export_leases() -> &'static ExportLeaseTable {
    static TABLE: OnceLock<ExportLeaseTable> = OnceLock::new();
    TABLE.get_or_init(ExportLeaseTable::default)
}

pub fn lease_ttl(seconds: i32) -> Duration {
    let secs = if seconds <= 0 {
        900
    } else {
        seconds.min(86_400)
    };
    Duration::from_secs(secs as u64)
}

/// Pick the node that should run snapshot / expose for this volume.
pub fn snapshot_node_id(vol: &VolumeRow) -> Result<String, String> {
    let class = host_volume::normalized_storage_class(&vol.storage_class);
    if host_volume::is_host(class) {
        if vol.host_node_id.is_empty() {
            return Err(format!("host volume '{}' has no host_node_id", vol.name));
        }
        Ok(vol.host_node_id.clone())
    } else {
        // Ceph: any healthy member; caller uses pick_healthy_ceph_node when empty.
        Ok(String::new())
    }
}

pub fn backend_handle_for(vol: &VolumeRow) -> String {
    host_volume::snapshot_backend_handle(
        host_volume::normalized_storage_class(&vol.storage_class),
        &vol.pool,
        &vol.image,
    )
}

pub fn storage_class_for(vol: &VolumeRow) -> String {
    host_volume::normalized_storage_class(&vol.storage_class).to_string()
}

/// Resolve the RBD/ZFS/LVM snap name stored in the DB row.
pub fn snap_name_on_backend(snap: &VolumeSnapshotRow) -> &str {
    &snap.rbd_snap
}

pub fn node_ready(node: &NodeRow) -> Result<(), String> {
    if node.status != "ready" {
        return Err(format!("node '{}' is not ready ({})", node.id, node.status));
    }
    if node.address.trim().is_empty() {
        return Err(format!("node '{}' has no address", node.id));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::VolumeRow;

    #[test]
    fn host_volume_requires_host_node() {
        let mut vol = VolumeRow::new_data("data", 1024);
        vol.storage_class = "zfs".into();
        assert!(snapshot_node_id(&vol).is_err());
        vol.host_node_id = "node-1".into();
        assert_eq!(snapshot_node_id(&vol).unwrap(), "node-1");
    }

    #[test]
    fn ceph_returns_empty_for_any_member() {
        let vol = VolumeRow::new_data("data", 1024);
        assert_eq!(snapshot_node_id(&vol).unwrap(), "");
    }

    #[test]
    fn lease_table_roundtrip() {
        let table = ExportLeaseTable::default();
        table.insert(ExportLease {
            lease_id: "l1".into(),
            node_id: "n1".into(),
            node_address: "https://n1:9091".into(),
            volume_name: "v".into(),
            snapshot_name: "s".into(),
            device_path: "/dev/nbd0".into(),
            size_bytes: 10,
            block_size: 4096,
            expires_at: Instant::now() + Duration::from_secs(60),
        });
        assert_eq!(table.get("l1").unwrap().device_path, "/dev/nbd0");
        assert!(table.remove("l1").is_some());
        assert!(table.get("l1").is_none());
    }
}
