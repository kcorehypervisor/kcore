//! ObjectStore (RGW) reconciler — scaffold: tick, phase transitions, optional
//! `ceph osd pool ls` / `radosgw-admin` probes when tools exist.

use std::time::Duration;

use tokio::time;
use tracing::{debug, info, warn};

use crate::ceph_cli::{self, CephCliError};
use crate::db::{Database, ObjectStoreRow, ObjectStoreStatusRow};
use crate::object_store_spec;

const RECONCILE_TICK: Duration = Duration::from_secs(20);

pub fn spawn_object_store_reconciler(db: Database) {
    tokio::spawn(async move {
        let mut ticker = time::interval(RECONCILE_TICK);
        ticker.tick().await;
        loop {
            ticker.tick().await;
            if let Err(error) = reconcile_once(&db).await {
                warn!(%error, "object store reconcile tick failed");
            }
        }
    });
}

async fn reconcile_once(db: &Database) -> Result<(), String> {
    for row in db
        .list_object_stores_needing_reconcile()
        .map_err(|e| e.to_string())?
    {
        if let Err(error) = reconcile_row(db, &row).await {
            warn!(store = %row.name, %error, "object store reconcile failed");
            let _ = db.upsert_object_store_status(&ObjectStoreStatusRow {
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

async fn reconcile_row(db: &Database, row: &ObjectStoreRow) -> Result<(), String> {
    let spec = object_store_spec::spec_from_json(&row.spec_json).map_err(|e| e.to_string())?;
    let rgw_id = object_store_spec::effective_rgw_id(&row.name, &spec);
    debug!(
        store = %row.name,
        %rgw_id,
        members = spec.members.len(),
        "object store reconcile tick"
    );

    db.upsert_object_store_status(&ObjectStoreStatusRow {
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

    match ceph_cli::run_ceph(&["version"]) {
        Ok(version) => {
            info!(store = %row.name, %version, "ceph reachable for object store scaffold");
            db.upsert_object_store_status(&ObjectStoreStatusRow {
                name: row.name.clone(),
                observed_generation: row.generation,
                phase: "bootstrapping".into(),
                health_message: format!(
                    "awaiting RGW deployment on members (rgw id '{rgw_id}', port {}); radosgw-admin not wired in scaffold",
                    spec.port
                ),
                last_transition_at: String::new(),
            })
            .map_err(|e| e.to_string())?;
        }
        Err(CephCliError::NotInstalled) => {
            db.upsert_object_store_status(&ObjectStoreStatusRow {
                name: row.name.clone(),
                observed_generation: row.generation,
                phase: "bootstrapping".into(),
                health_message: "ceph CLI not available; ObjectStore recorded only (lab scaffold)"
                    .into(),
                last_transition_at: String::new(),
            })
            .map_err(|e| e.to_string())?;
        }
        Err(CephCliError::Failed(detail)) => {
            db.upsert_object_store_status(&ObjectStoreStatusRow {
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
