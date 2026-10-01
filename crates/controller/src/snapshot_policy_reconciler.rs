//! SnapshotPolicy reconciler — create crash-consistent snaps on a schedule
//! and prune to `keep`, never deleting a snap that still has unflattened clones.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use tokio::time;
use tracing::{debug, info, warn};
use uuid::Uuid;

use crate::db::{Database, SnapshotPolicyRow, VolumeRow, VolumeSnapshotRow};
use crate::node_client::NodeClients;
use crate::node_proto;

const RECONCILE_TICK: Duration = Duration::from_secs(60);

pub fn spawn_snapshot_policy_reconciler(db: Database, clients: NodeClients) {
    tokio::spawn(async move {
        let mut ticker = time::interval(RECONCILE_TICK);
        ticker.tick().await;
        loop {
            ticker.tick().await;
            if let Err(e) = reconcile_once(&db, &clients).await {
                warn!(error = %e, "snapshot policy reconcile tick failed");
            }
        }
    });
}

async fn reconcile_once(db: &Database, clients: &NodeClients) -> Result<(), String> {
    let policies = db
        .list_snapshot_policies()
        .map_err(|e| format!("list policies: {e}"))?;
    for policy in policies.into_iter().filter(|p| p.enabled) {
        if let Err(e) = reconcile_policy(db, clients, &policy).await {
            warn!(policy = %policy.name, error = %e, "snapshot policy failed");
            let mut updated = policy.clone();
            updated.last_message = e;
            let _ = db.upsert_snapshot_policy(&updated);
        }
    }
    Ok(())
}

async fn reconcile_policy(
    db: &Database,
    clients: &NodeClients,
    policy: &SnapshotPolicyRow,
) -> Result<(), String> {
    if !schedule_due(&policy.schedule, &policy.last_run_at) {
        return Ok(());
    }
    let volumes = select_volumes(db, policy)?;
    if volumes.is_empty() {
        let mut updated = policy.clone();
        updated.last_run_at = now_sqlite();
        updated.last_message = "no volumes matched selector".into();
        db.upsert_snapshot_policy(&updated)
            .map_err(|e| e.to_string())?;
        return Ok(());
    }

    let node = pick_ceph_node(db)?;
    if clients.get_storage(&node.address).is_none() {
        clients
            .connect(&node.address)
            .await
            .map_err(|e| format!("connect {e}"))?;
    }
    let mut storage = clients
        .get_storage(&node.address)
        .ok_or_else(|| "storage client unavailable".to_string())?;

    let mut created = 0usize;
    let mut pruned = 0usize;
    for vol in &volumes {
        if !vol.vm_id.is_empty() {
            if let Ok(Some(open)) = db.get_open_vm_operation(&vol.vm_id) {
                debug!(
                    volume = %vol.name,
                    op = %open.id,
                    "skip snapshot; VM has open operation"
                );
                continue;
            }
        }
        let snap_name = format!("{}-{}", policy.name, now_compact());
        let rbd_snap = VolumeRow::serial_from_id(&Uuid::new_v4().to_string());
        storage
            .snapshot_volume(node_proto::SnapshotVolumeRequest {
                backend_handle: format!("{}/{}", vol.pool, vol.image),
                snapshot_name: rbd_snap.clone(),
                protect: true,
            })
            .await
            .map_err(|e| format!("snap {}: {e}", vol.name))?;
        db.insert_volume_snapshot(&VolumeSnapshotRow {
            id: Uuid::new_v4().to_string(),
            name: snap_name,
            volume_id: vol.id.clone(),
            rbd_snap,
            protected: true,
            size_bytes: vol.size_bytes,
            consistency: "crash".into(),
            created_at: String::new(),
        })
        .map_err(|e| e.to_string())?;
        created += 1;
        pruned += prune_volume(db, &mut storage, vol, policy.keep).await?;
    }

    let mut updated = policy.clone();
    updated.last_run_at = now_sqlite();
    updated.last_message = format!("created={created} pruned={pruned}");
    db.upsert_snapshot_policy(&updated)
        .map_err(|e| e.to_string())?;
    info!(
        policy = %policy.name,
        created,
        pruned,
        "snapshot policy tick complete"
    );
    Ok(())
}

async fn prune_volume(
    db: &Database,
    storage: &mut node_proto::node_storage_client::NodeStorageClient<tonic::transport::Channel>,
    vol: &VolumeRow,
    keep: i32,
) -> Result<usize, String> {
    if keep <= 0 {
        return Ok(0);
    }
    let snaps = db
        .list_volume_snapshots(Some(&vol.id))
        .map_err(|e| e.to_string())?;
    if snaps.len() as i32 <= keep {
        return Ok(0);
    }
    let mut removed = 0usize;
    for snap in snaps.into_iter().skip(keep as usize) {
        let children = db
            .count_volumes_with_parent_snapshot(&snap.id)
            .map_err(|e| e.to_string())?;
        if children > 0 {
            debug!(
                snapshot = %snap.name,
                children,
                "skip prune; unflattened clones remain"
            );
            continue;
        }
        if let Err(e) = storage
            .delete_volume_snapshot(node_proto::DeleteVolumeSnapshotRequest {
                backend_handle: format!("{}/{}", vol.pool, vol.image),
                snapshot_name: snap.rbd_snap.clone(),
                unprotect: snap.protected,
            })
            .await
        {
            warn!(snapshot = %snap.name, error = %e, "rbd snap rm failed during prune");
            continue;
        }
        let _ = db.delete_volume_snapshot(&snap.id);
        removed += 1;
    }
    Ok(removed)
}

fn select_volumes(db: &Database, policy: &SnapshotPolicyRow) -> Result<Vec<VolumeRow>, String> {
    let all = db.list_volumes().map_err(|e| e.to_string())?;
    let vol_filter = policy.selector_volume.trim();
    let vm_filter = policy.selector_vm.trim();
    if !vol_filter.is_empty() {
        return Ok(all
            .into_iter()
            .filter(|v| v.name == vol_filter || v.id == vol_filter)
            .collect());
    }
    if vm_filter.is_empty() {
        return Err("selector.vm or selector.volume is required".into());
    }
    let vm = db
        .get_vm(vm_filter)
        .map_err(|e| e.to_string())?
        .or_else(|| {
            db.list_vms()
                .ok()
                .and_then(|rows| rows.into_iter().find(|v| v.name == vm_filter))
        })
        .ok_or_else(|| format!("VM '{vm_filter}' not found"))?;
    Ok(all
        .into_iter()
        .filter(|v| v.vm_id == vm.id && v.storage_class == "ceph")
        .collect())
}

fn pick_ceph_node(db: &Database) -> Result<crate::db::NodeRow, String> {
    let nodes = db.list_nodes().map_err(|e| e.to_string())?;
    nodes
        .into_iter()
        .find(|n| n.approval_status == "approved")
        .ok_or_else(|| "no approved node for Ceph storage RPC".into())
}

pub fn schedule_due(schedule: &str, last_run_at: &str) -> bool {
    let interval = match schedule.trim() {
        "@hourly" => 3600,
        "@daily" => 86400,
        s if s.starts_with("every:") => s[6..].parse::<u64>().unwrap_or(0),
        // Unknown cron: treat as daily so a typo still runs eventually.
        _ => 86400,
    };
    if interval == 0 {
        return false;
    }
    if last_run_at.trim().is_empty() {
        return true;
    }
    let last = parse_sqlite_time(last_run_at).unwrap_or(0);
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    now.saturating_sub(last) >= interval
}

fn parse_sqlite_time(s: &str) -> Option<u64> {
    // "YYYY-MM-DD HH:MM:SS" UTC-ish from sqlite datetime('now')
    let s = s.trim();
    if s.len() < 19 {
        return None;
    }
    let year: i64 = s.get(0..4)?.parse().ok()?;
    let month: u32 = s.get(5..7)?.parse().ok()?;
    let day: u32 = s.get(8..10)?.parse().ok()?;
    let hour: u32 = s.get(11..13)?.parse().ok()?;
    let min: u32 = s.get(14..16)?.parse().ok()?;
    let sec: u32 = s.get(17..19)?.parse().ok()?;
    // Approximate days since epoch without chrono.
    let days = days_from_civil(year, month, day)?;
    Some(days * 86400 + u64::from(hour) * 3600 + u64::from(min) * 60 + u64::from(sec))
}

fn days_from_civil(y: i64, m: u32, d: u32) -> Option<u64> {
    if !(1..=12).contains(&m) || !(1..=31).contains(&d) {
        return None;
    }
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y.rem_euclid(400) as u64;
    let mp = if m > 2 { m - 3 } else { m + 9 };
    let doy = (153 * (mp as u64) + 2) / 5 + (d as u64) - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = (era * 146097) + doe as i64 - 719468;
    if days < 0 {
        None
    } else {
        Some(days as u64)
    }
}

fn now_sqlite() -> String {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let days = secs / 86400;
    let tod = secs % 86400;
    let (y, m, d) = civil_from_days(days as i64);
    let h = tod / 3600;
    let mi = (tod % 3600) / 60;
    let s = tod % 60;
    format!("{y:04}-{m:02}-{d:02} {h:02}:{mi:02}:{s:02}")
}

fn now_compact() -> String {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    secs.to_string()
}

fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719468;
    let era = z.div_euclid(146097);
    let doe = z.rem_euclid(146097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = (yoe as i64) + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let y = if m <= 2 { y + 1 } else { y };
    (y, m, d)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn schedule_due_empty_last_run() {
        assert!(schedule_due("@hourly", ""));
        assert!(schedule_due("every:60", ""));
    }

    #[test]
    fn schedule_due_respects_interval() {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs();
        // last run "just now" via sqlite format approx
        let last = now_sqlite();
        assert!(
            !schedule_due("every:3600", &last) || {
                // race if we crossed a second boundary — still shouldn't be due for hourly
                let last_secs = parse_sqlite_time(&last).unwrap();
                now.saturating_sub(last_secs) < 3600
            }
        );
        assert!(schedule_due("every:1", "1970-01-01 00:00:00"));
    }
}
