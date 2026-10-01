//! SharedFilesystem (CephFS) reconciler — scaffold: tick, phase transitions,
//! optional local `ceph fs` probes when the CLI is present.

use std::time::Duration;

use tokio::time;
use tracing::{debug, info, warn};

use crate::ceph_cli::{self, CephCliError};
use crate::db::{Database, SharedFilesystemRow, SharedFilesystemStatusRow};
use crate::shared_filesystem_spec;

const RECONCILE_TICK: Duration = Duration::from_secs(20);

pub fn spawn_shared_filesystem_reconciler(db: Database) {
    tokio::spawn(async move {
        let mut ticker = time::interval(RECONCILE_TICK);
        ticker.tick().await;
        loop {
            ticker.tick().await;
            if let Err(error) = reconcile_once(&db).await {
                warn!(%error, "shared filesystem reconcile tick failed");
            }
        }
    });
}

async fn reconcile_once(db: &Database) -> Result<(), String> {
    for row in db
        .list_shared_filesystems_needing_reconcile()
        .map_err(|e| e.to_string())?
    {
        if let Err(error) = reconcile_row(db, &row).await {
            warn!(filesystem = %row.name, %error, "shared filesystem reconcile failed");
            let _ = db.upsert_shared_filesystem_status(&SharedFilesystemStatusRow {
                name: row.name.clone(),
                observed_generation: row.generation,
                phase: "failed".into(),
                health_message: error,
                last_transition_at: String::new(),
            });
        }
    }
    Ok(())
}

async fn reconcile_row(db: &Database, row: &SharedFilesystemRow) -> Result<(), String> {
    let spec = shared_filesystem_spec::spec_from_json(&row.spec_json).map_err(|e| e.to_string())?;
    let fs_name = shared_filesystem_spec::effective_fs_name(&row.name, &spec);
    debug!(filesystem = %row.name, %fs_name, "shared filesystem reconcile tick");

    db.upsert_shared_filesystem_status(&SharedFilesystemStatusRow {
        name: row.name.clone(),
        observed_generation: 0,
        phase: "bootstrapping".into(),
        health_message: String::new(),
        last_transition_at: String::new(),
    })
    .map_err(|e| e.to_string())?;

    if db
        .get_ceph_cluster(&spec.ceph_cluster)
        .map_err(|e| e.to_string())?
        .is_none()
    {
        return Err(format!(
            "cephCluster '{}' is not registered",
            spec.ceph_cluster
        ));
    }

    match ceph_cli::run_ceph(&["fs", "ls", "--format", "json"]) {
        Ok(raw) if raw.contains(&format!("\"name\":\"{fs_name}\"")) || raw.contains(&fs_name) => {
            info!(filesystem = %row.name, "cephfs volume present (stub reconcile)");
            db.upsert_shared_filesystem_status(&SharedFilesystemStatusRow {
                name: row.name.clone(),
                observed_generation: row.generation,
                phase: "healthy".into(),
                health_message: "CephFS volume reported by ceph fs ls".into(),
                last_transition_at: String::new(),
            })
            .map_err(|e| e.to_string())?;
        }
        Ok(_) => {
            let msg = format!(
                "awaiting CephFS bootstrap for '{fs_name}' (ceph fs new / pools — not automated in scaffold)"
            );
            db.upsert_shared_filesystem_status(&SharedFilesystemStatusRow {
                name: row.name.clone(),
                observed_generation: row.generation,
                phase: "bootstrapping".into(),
                health_message: msg,
                last_transition_at: String::new(),
            })
            .map_err(|e| e.to_string())?;
        }
        Err(CephCliError::NotInstalled) => {
            db.upsert_shared_filesystem_status(&SharedFilesystemStatusRow {
                name: row.name.clone(),
                observed_generation: row.generation,
                phase: "bootstrapping".into(),
                health_message:
                    "ceph CLI not available; SharedFilesystem recorded only (lab scaffold)".into(),
                last_transition_at: String::new(),
            })
            .map_err(|e| e.to_string())?;
        }
        Err(CephCliError::Failed(detail)) => {
            db.upsert_shared_filesystem_status(&SharedFilesystemStatusRow {
                name: row.name.clone(),
                observed_generation: row.generation,
                phase: "degraded".into(),
                health_message: detail,
                last_transition_at: String::new(),
            })
            .map_err(|e| e.to_string())?;
        }
    }
    Ok(())
}
