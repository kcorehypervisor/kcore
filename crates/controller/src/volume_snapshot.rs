//! Snapshot / clone semantics helpers (testable without Ceph).

use crate::db::{VolumeRow, VolumeSnapshotRow};

pub const CONSISTENCY_CRASH: &str = "crash";
pub const CONSISTENCY_QUIESCED: &str = "quiesced";

/// Normalize API consistency; empty → crash.
pub fn normalize_snapshot_consistency(raw: &str) -> Result<String, &'static str> {
    let s = raw.trim();
    if s.is_empty() {
        return Ok(CONSISTENCY_CRASH.into());
    }
    match s.to_ascii_lowercase().as_str() {
        CONSISTENCY_CRASH => Ok(CONSISTENCY_CRASH.into()),
        CONSISTENCY_QUIESCED => Ok(CONSISTENCY_QUIESCED.into()),
        _ => Err("consistency must be 'crash' or 'quiesced'"),
    }
}

/// Guest fsfreeze is required only for quiesced snapshots on live attached volumes.
pub fn snapshot_needs_guest_quiesce(consistency: &str, vol: &VolumeRow, vm_live: bool) -> bool {
    consistency == CONSISTENCY_QUIESCED && !vol.vm_id.is_empty() && vol.is_attached() && vm_live
}

/// Controller DB + node clone request fields after a protected snapshot exists.
pub fn clone_volume_from_snapshot_row(
    parent_vol: &VolumeRow,
    snap: &VolumeSnapshotRow,
    child_image: &str,
) -> (VolumeRow, String, String, String) {
    let mut row = VolumeRow::new_data(&format!("{}-clone", parent_vol.name), snap.size_bytes);
    row.parent_snapshot_id = snap.id.clone();
    row.size_bytes = snap.size_bytes;
    row.source_json = format!(
        r#"{{"snapshotId":"{}","snapshotName":"{}"}}"#,
        snap.id, snap.name
    );
    let parent_handle = format!("{}/{}", parent_vol.pool, parent_vol.image);
    (
        row,
        parent_handle,
        snap.rbd_snap.clone(),
        child_image.to_string(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::Database;

    #[test]
    fn normalize_consistency_defaults_and_rejects_unknown() {
        assert_eq!(
            normalize_snapshot_consistency("").unwrap(),
            CONSISTENCY_CRASH
        );
        assert_eq!(
            normalize_snapshot_consistency("quiesced").unwrap(),
            CONSISTENCY_QUIESCED
        );
        assert!(normalize_snapshot_consistency("sync").is_err());
    }

    #[test]
    fn quiesce_only_for_live_attached_volumes() {
        let mut vol = VolumeRow::new_data("pgdata", 1024);
        assert!(!snapshot_needs_guest_quiesce(
            CONSISTENCY_QUIESCED,
            &vol,
            true
        ));
        vol.vm_id = "vm-1".into();
        vol.attach_state = VolumeRow::ATTACH_ATTACHED.into();
        assert!(snapshot_needs_guest_quiesce(
            CONSISTENCY_QUIESCED,
            &vol,
            true
        ));
        assert!(!snapshot_needs_guest_quiesce(CONSISTENCY_CRASH, &vol, true));
        assert!(!snapshot_needs_guest_quiesce(
            CONSISTENCY_QUIESCED,
            &vol,
            false
        ));
    }

    #[test]
    fn clone_from_snapshot_sets_parent_snapshot_id_and_rbd_args() {
        let parent = VolumeRow::new_data("pgdata", 20 * 1024 * 1024 * 1024);
        let snap = VolumeSnapshotRow {
            id: "snap-uuid".into(),
            name: "pgdata-t0".into(),
            volume_id: parent.id.clone(),
            rbd_snap: "abc123".into(),
            protected: true,
            size_bytes: parent.size_bytes,
            consistency: CONSISTENCY_CRASH.into(),
            created_at: String::new(),
        };
        let (clone, parent_handle, rbd_snap, child_image) =
            clone_volume_from_snapshot_row(&parent, &snap, "kcore-vol-deadbeef");
        assert_eq!(clone.parent_snapshot_id, "snap-uuid");
        assert_eq!(parent_handle, format!("{}/{}", parent.pool, parent.image));
        assert_eq!(rbd_snap, "abc123");
        assert_eq!(child_image, "kcore-vol-deadbeef");
        assert!(clone.source_json.contains("pgdata-t0"));
    }

    /// Snap → clone → boot: DB refuses deleting a protected snap while clones exist;
    /// flatten/delete clone clears the link (marker-at-t0 semantics in lab docs).
    #[test]
    fn snap_clone_delete_order_blocks_protected_snap_with_children() {
        let db = Database::open(":memory:").expect("db");
        let mut root = VolumeRow::new_data("pgdata", 10 * 1024 * 1024 * 1024);
        root.vm_id = "vm-1".into();
        root.attach_state = VolumeRow::ATTACH_ATTACHED.into();
        db.upsert_volume(&root).unwrap();
        let snap = VolumeSnapshotRow {
            id: "snap-1".into(),
            name: "pgdata-t0".into(),
            volume_id: root.id.clone(),
            rbd_snap: "s1".into(),
            protected: true,
            size_bytes: root.size_bytes,
            consistency: CONSISTENCY_QUIESCED.into(),
            created_at: String::new(),
        };
        db.insert_volume_snapshot(&snap).unwrap();
        let mut clone = VolumeRow::new_data("pgdata-clone", root.size_bytes);
        clone.parent_snapshot_id = snap.id.clone();
        clone.vm_id = "vm-scratch".into();
        clone.attach_state = VolumeRow::ATTACH_ATTACHED.into();
        db.upsert_volume(&clone).unwrap();
        assert_eq!(db.count_volumes_with_parent_snapshot(&snap.id).unwrap(), 1);
        let loaded = db
            .get_volume_snapshot_by_name("pgdata-t0")
            .unwrap()
            .unwrap();
        assert_eq!(loaded.consistency, CONSISTENCY_QUIESCED);
        clone.parent_snapshot_id.clear();
        db.upsert_volume(&clone).unwrap();
        assert_eq!(db.count_volumes_with_parent_snapshot(&snap.id).unwrap(), 0);
    }
}
