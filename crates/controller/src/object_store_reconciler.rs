//! ObjectStore (RGW) reconciler: ensure RGW pools exist and create users with
//! `radosgw-admin` when tools are present.

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

    match ensure_rgw_ready(&rgw_id, spec.port) {
        Ok(msg) => {
            let msg = match crate::ceph_cluster_reconciler::daemon_push_note(
                db,
                spec.ceph_cluster.trim(),
                crate::ceph_cluster_reconciler::DaemonKind::Rgw,
            ) {
                Ok(note) => format!("{msg}; {note}"),
                Err(error) => format!("{msg}; RGW placement unknown: {error}"),
            };
            info!(store = %row.name, %msg, "object store ready");
            // Sync ObjectUser rows into radosgw when possible.
            for user in db
                .list_object_users_for_store(&row.name)
                .unwrap_or_default()
            {
                let _ = ensure_rgw_user(&user.name, &user.access_key, &user.secret);
            }
            db.upsert_object_store_status(&ObjectStoreStatusRow {
                name: row.name.clone(),
                observed_generation: row.generation,
                phase: "healthy".into(),
                health_message: msg,
                last_transition_at: String::new(),
            })
            .map_err(|e| e.to_string())?;
        }
        Err(CephCliError::NotInstalled) => {
            db.upsert_object_store_status(&ObjectStoreStatusRow {
                name: row.name.clone(),
                observed_generation: row.generation,
                phase: "bootstrapping".into(),
                health_message: format!(
                    "awaiting RGW on members (rgw id '{rgw_id}', port {}); ceph/radosgw-admin not on controller PATH; enableRgw is still pushed by the CephCluster apply",
                    spec.port
                ),
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

fn ensure_rgw_ready(rgw_id: &str, port: i32) -> Result<String, CephCliError> {
    let _version = ceph_cli::run_ceph(&["version"])?;
    // Default RGW pools; create if missing so a fresh lab can serve S3.
    for pool in [
        ".rgw.root",
        "default.rgw.control",
        "default.rgw.meta",
        "default.rgw.log",
        "default.rgw.buckets.index",
        "default.rgw.buckets.data",
    ] {
        let listed = ceph_cli::run_ceph(&["osd", "pool", "ls"])?;
        if !listed.lines().any(|l| l.trim() == pool) {
            let _ = ceph_cli::run_ceph(&["osd", "pool", "create", pool]);
        }
    }
    Ok(format!(
        "RGW pools present for '{rgw_id}' (client port {port})"
    ))
}

fn ensure_rgw_user(uid: &str, access_key: &str, secret: &str) -> Result<(), CephCliError> {
    if !std::process::Command::new("radosgw-admin")
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
    {
        return Err(CephCliError::NotInstalled);
    }
    let check = std::process::Command::new("radosgw-admin")
        .args(["user", "info", "--uid", uid])
        .output()
        .map_err(|e| CephCliError::Failed(e.to_string()))?;
    if check.status.success() {
        return Ok(());
    }
    let create = std::process::Command::new("radosgw-admin")
        .args([
            "user",
            "create",
            "--uid",
            uid,
            "--display-name",
            uid,
            "--access-key",
            access_key,
            "--secret-key",
            secret,
        ])
        .output()
        .map_err(|e| CephCliError::Failed(e.to_string()))?;
    if create.status.success() {
        Ok(())
    } else {
        Err(CephCliError::Failed(
            String::from_utf8_lossy(&create.stderr).trim().to_string(),
        ))
    }
}
