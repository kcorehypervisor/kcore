//! SharedFilesystem (CephFS) reconciler: create data/metadata pools, `ceph fs new`,
//! authorize clients when the local `ceph` CLI can reach the cluster.

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

    let meta_pool = format!("{fs_name}-metadata");
    let data_pool = format!("{fs_name}-data");

    match ensure_cephfs(&fs_name, &meta_pool, &data_pool, &spec) {
        Ok(msg) => {
            let msg = match crate::ceph_cluster_reconciler::daemon_push_note(
                db,
                spec.ceph_cluster.trim(),
                crate::ceph_cluster_reconciler::DaemonKind::Mds,
            ) {
                Ok(note) => format!("{msg}; {note}"),
                Err(error) => format!("{msg}; MDS placement unknown: {error}"),
            };
            info!(filesystem = %row.name, %msg, "cephfs ready");
            db.upsert_shared_filesystem_status(&SharedFilesystemStatusRow {
                name: row.name.clone(),
                observed_generation: row.generation,
                phase: "healthy".into(),
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
                    "ceph CLI not available on the controller; SharedFilesystem recorded only"
                        .into(),
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

fn ensure_cephfs(
    fs_name: &str,
    meta_pool: &str,
    data_pool: &str,
    spec: &crate::controller_proto::SharedFilesystemSpec,
) -> Result<String, CephCliError> {
    let listed = ceph_cli::run_ceph(&["fs", "ls", "--format", "json"])?;
    if listed.contains(&format!("\"name\":\"{fs_name}\"")) || listed.contains(fs_name) {
        authorize_clients(fs_name, spec)?;
        return Ok(format!("CephFS '{fs_name}' present; clients authorized"));
    }

    ensure_pool(meta_pool, 32)?;
    ensure_pool(data_pool, 32)?;
    let _ = ceph_cli::run_ceph(&["osd", "pool", "application", "enable", meta_pool, "cephfs"]);
    let _ = ceph_cli::run_ceph(&["osd", "pool", "application", "enable", data_pool, "cephfs"]);
    ceph_cli::run_ceph(&["fs", "new", fs_name, meta_pool, data_pool])?;
    if spec.quota_bytes > 0 {
        let q = spec.quota_bytes.to_string();
        let _ = ceph_cli::run_ceph(&["fs", "set", fs_name, "max_bytes", &q]);
    }
    authorize_clients(fs_name, spec)?;
    Ok(format!(
        "created CephFS '{fs_name}' on pools {meta_pool}/{data_pool}"
    ))
}

fn ensure_pool(name: &str, pg_num: i32) -> Result<(), CephCliError> {
    let listed = ceph_cli::run_ceph(&["osd", "pool", "ls"])?;
    if listed.lines().any(|l| l.trim() == name) {
        return Ok(());
    }
    let pg = pg_num.to_string();
    ceph_cli::run_ceph(&["osd", "pool", "create", name, &pg]).map(|_| ())
}

fn authorize_clients(
    fs_name: &str,
    spec: &crate::controller_proto::SharedFilesystemSpec,
) -> Result<(), CephCliError> {
    for client in &spec.clients {
        let name = client.name.trim();
        if name.is_empty() {
            continue;
        }
        let entity = if name.starts_with("client.") {
            name.to_string()
        } else {
            format!("client.{name}")
        };
        let paths = if client.paths.is_empty() {
            vec!["/".to_string()]
        } else {
            client.paths.clone()
        };
        for path in paths {
            let _ = ceph_cli::run_ceph(&["fs", "authorize", fs_name, &entity, &path, "rw"]);
        }
    }
    Ok(())
}
