//! CephCluster reconciler — push Nix, bootstrap keys/MON/MGR/OSD, ensure RBD pool.

use std::collections::HashSet;
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

use tokio::time;
use tonic::Request;
use tracing::warn;

use crate::ceph_cluster_spec;
use crate::db::{CephClusterRow, CephClusterStatusRow, Database};
use crate::node_client::NodeClients;
use crate::node_proto;
use crate::webhooks;
use kcore_sanitize::nix_escape;

static CEPH_WEBHOOKS: OnceLock<webhooks::Dispatcher> = OnceLock::new();

/// Wire the Ceph reconciler to the same webhook dispatcher the rest of the
/// controller uses. Safe to call once after `Dispatcher::spawn`.
pub fn set_webhooks(dispatcher: webhooks::Dispatcher) {
    let _ = CEPH_WEBHOOKS.set(dispatcher);
}

fn maybe_emit_ceph_health_webhook(
    cluster: &str,
    previous_message: Option<&str>,
    status: &str,
    message: &str,
) {
    let left_ok = status != "HEALTH_OK"
        && previous_message
            .map(|m| m.starts_with("HEALTH_OK"))
            .unwrap_or(true);
    if !left_ok {
        return;
    }
    let Some(hooks) = CEPH_WEBHOOKS.get() else {
        return;
    };
    hooks.emit(
        webhooks::EVENT_CEPH_HEALTH,
        format!("ceph-cluster/{cluster}"),
        serde_json::json!({
            "cluster": cluster,
            "health": status,
            "message": message,
        }),
    );
}

const RECONCILE_TICK: Duration = Duration::from_secs(15);
const CEPH_CLUSTER_NAME: &str = "ceph";
const RBD_POOL: &str = "kcore-vms";
const NIX_APPLY_WAIT: Duration = Duration::from_secs(600);
/// Returned when health was recorded and the caller must not overwrite it
/// with phase=failed.
const NOT_HEALTHY_YET: &str = "ceph not healthy yet:";
/// Marker in `health_message` while the controller holds `norebalance` after
/// adding OSDs.
const BACKFILL_RAMP_NOTE: &str = "backfill ramp:";

/// Clusters where this controller set `norebalance` after creating OSDs and
/// has not released it yet. Survives a failed pass (which overwrites the
/// status message); a controller restart falls back to the message marker.
fn backfill_ramp() -> &'static Mutex<HashSet<String>> {
    static RAMP: OnceLock<Mutex<HashSet<String>>> = OnceLock::new();
    RAMP.get_or_init(|| Mutex::new(HashSet::new()))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DaemonKind {
    Mds,
    Rgw,
}

/// One-line status for a SharedFilesystem/ObjectStore: where the controller
/// pushes `enableMds`/`enableRgw`, and whether the CephCluster pass that
/// carries it has completed.
pub(crate) fn daemon_push_note(
    db: &Database,
    cluster: &str,
    kind: DaemonKind,
) -> Result<String, String> {
    let (option, daemon) = match kind {
        DaemonKind::Mds => ("enableMds", "MDS"),
        DaemonKind::Rgw => ("enableRgw", "RGW"),
    };
    let Some(row) = db.get_ceph_cluster(cluster).map_err(|e| e.to_string())? else {
        return Ok(format!("cephCluster '{cluster}' is not registered"));
    };
    let spec = ceph_cluster_spec::spec_from_json(&row.spec_json).map_err(|e| e.to_string())?;
    let service_flags = ceph_service_flags(db, cluster)?;
    let targets: Vec<String> = spec
        .nodes
        .iter()
        .filter(|n| match kind {
            DaemonKind::Mds => service_flags.enable_mds_for(&n.node_id, &n.role),
            DaemonKind::Rgw => service_flags.enable_rgw_for(&n.node_id, &n.role),
        })
        .map(|n| n.node_id.clone())
        .collect();
    if targets.is_empty() {
        return Ok(format!(
            "no member of '{cluster}' can host {daemon} (osd-only or not listed); {option} is not set anywhere"
        ));
    }
    let applied = db
        .get_ceph_cluster_status(cluster)
        .map_err(|e| e.to_string())?
        .is_some_and(|s| s.observed_generation >= row.generation);
    let list = targets.join(",");
    Ok(if applied {
        format!(
            "{daemon} enabled on {list} (kcore.ceph.{option} pushed by CephCluster '{cluster}')"
        )
    } else {
        format!("{daemon} queued for {list}: CephCluster '{cluster}' is still applying kcore.ceph.{option}")
    })
}

pub fn spawn_ceph_cluster_reconciler(db: Database, clients: NodeClients) {
    tokio::spawn(async move {
        let mut ticker = time::interval(RECONCILE_TICK);
        ticker.tick().await;
        loop {
            ticker.tick().await;
            if let Err(error) = reconcile_once(&db, &clients).await {
                warn!(%error, "ceph cluster reconcile tick failed");
            }
        }
    });
}

async fn reconcile_once(db: &Database, clients: &NodeClients) -> Result<(), String> {
    for row in db
        .list_ceph_clusters_needing_reconcile()
        .map_err(|e| e.to_string())?
    {
        if let Err(error) = reconcile_row(db, clients, &row).await {
            if error.starts_with(NOT_HEALTHY_YET) {
                // reconcile_row already stored degraded/failed from `ceph -s`.
                // Stamping phase=failed here turned every HEALTH_WARN into a
                // hard failure and hid the real health string.
                warn!(%error, cluster = %row.name, "ceph cluster not healthy yet");
                continue;
            }
            let _ = db.upsert_ceph_cluster_status(&CephClusterStatusRow {
                name: row.name.clone(),
                observed_generation: 0,
                phase: "failed".into(),
                health_message: error.clone(),
                ceph_status_json: String::new(),
                last_transition_at: String::new(),
            });
        }
    }
    Ok(())
}

pub(crate) async fn admin_for(
    db: &Database,
    clients: &NodeClients,
    node_id: &str,
) -> Result<
    crate::node_proto::node_admin_client::NodeAdminClient<crate::node_client::NodeChannel>,
    String,
> {
    let node = db
        .get_node(node_id)
        .map_err(|e| e.to_string())?
        .ok_or_else(|| format!("node {node_id} is not registered"))?;
    if clients.get_admin(&node.address).is_none() {
        clients
            .connect(&node.address)
            .await
            .map_err(|e| e.to_string())?;
    }
    clients
        .get_admin(&node.address)
        .ok_or_else(|| format!("no admin client for {}", node.address))
}

pub(crate) fn mon_map_string(spec: &crate::controller_proto::CephClusterSpec) -> String {
    spec.nodes
        .iter()
        .map(|n| {
            let daemon = sanitize_daemon_id(&n.node_id, 0);
            let ip = n
                .mon_addr
                .split(':')
                .next()
                .unwrap_or(n.mon_addr.as_str())
                .trim();
            format!("{daemon}={ip}")
        })
        .collect::<Vec<_>>()
        .join(",")
}

async fn reconcile_row(
    db: &Database,
    clients: &NodeClients,
    row: &CephClusterRow,
) -> Result<(), String> {
    let spec = ceph_cluster_spec::spec_from_json(&row.spec_json).map_err(|e| e.to_string())?;
    if ceph_cluster_spec::active_external(&spec).is_some() {
        return crate::ceph_external::reconcile(db, clients, row, &spec).await;
    }
    if db
        .get_ceph_cluster_status(&row.name)
        .map_err(|e| e.to_string())?
        .is_some_and(|s| s.health_message.contains(BACKFILL_RAMP_NOTE))
    {
        backfill_ramp()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(row.name.clone());
    }
    db.upsert_ceph_cluster_status(&CephClusterStatusRow {
        name: row.name.clone(),
        observed_generation: 0,
        phase: "bootstrapping".into(),
        health_message: String::new(),
        ceph_status_json: String::new(),
        last_transition_at: String::new(),
    })
    .map_err(|e| e.to_string())?;

    let mon_map = mon_map_string(&spec);
    let mut bootstrap = row.bootstrap_json.clone().into_bytes();
    let service_flags = ceph_service_flags(db, &row.name)?;

    for (index, node) in spec.nodes.iter().enumerate() {
        let daemon_id = sanitize_daemon_id(&node.node_id, index);
        let mut admin = admin_for(db, clients, &node.node_id).await?;
        let nix_extras = service_flags.extras_for(&spec, node);
        let mut applied = admin
            .apply_ceph_config(Request::new(node_proto::ApplyCephConfigRequest {
                ceph_nix: generate_ceph_nix(&spec, node, &daemon_id, &mon_map, &nix_extras),
                rebuild: true,
                fsid: spec.fsid.clone(),
                mon: true,
                mgr: true,
                osd: true,
                daemon_id: daemon_id.clone(),
                keyring: bootstrap.clone(),
                mon_map: mon_map.clone(),
                public_addr: node
                    .mon_addr
                    .split(':')
                    .next()
                    .unwrap_or(&node.mon_addr)
                    .to_string(),
                cluster_addr: node.cluster_addr.clone(),
                allow_found: bootstrap.is_empty() && index == 0,
            }))
            .await
            .map_err(|e| e.to_string())?
            .into_inner();
        if !applied.success {
            return Err(applied.message);
        }
        if !applied.keyring.is_empty() && bootstrap.is_empty() {
            bootstrap = applied.keyring.clone();
            let bootstrap_str = String::from_utf8_lossy(&bootstrap).to_string();
            db.set_ceph_cluster_bootstrap(&row.name, &bootstrap_str)
                .map_err(|e| e.to_string())?;
        }
        if !applied.daemons_started {
            if !applied.apply_id.is_empty() {
                wait_for_nix_apply(&mut admin, &applied.apply_id).await?;
                admin = admin_for(db, clients, &node.node_id).await?;
            }
            applied = admin
                .apply_ceph_config(Request::new(node_proto::ApplyCephConfigRequest {
                    ceph_nix: generate_ceph_nix(&spec, node, &daemon_id, &mon_map, &nix_extras),
                    rebuild: false,
                    fsid: spec.fsid.clone(),
                    mon: true,
                    mgr: true,
                    osd: true,
                    daemon_id: daemon_id.clone(),
                    keyring: bootstrap.clone(),
                    mon_map: mon_map.clone(),
                    public_addr: node
                        .mon_addr
                        .split(':')
                        .next()
                        .unwrap_or(&node.mon_addr)
                        .to_string(),
                    cluster_addr: node.cluster_addr.clone(),
                    allow_found: false,
                }))
                .await
                .map_err(|e| e.to_string())?
                .into_inner();
            if !applied.success || !applied.daemons_started {
                return Err(if applied.message.is_empty() {
                    format!("mon/mgr did not start on {}", node.node_id)
                } else {
                    applied.message
                });
            }
            if !applied.keyring.is_empty() && bootstrap.is_empty() {
                bootstrap = applied.keyring.clone();
                let bootstrap_str = String::from_utf8_lossy(&bootstrap).to_string();
                db.set_ceph_cluster_bootstrap(&row.name, &bootstrap_str)
                    .map_err(|e| e.to_string())?;
            }
        }

        let devices = ceph_cluster_spec::osd_devices(node);
        let osd = admin
            .reconcile_ceph_osds(Request::new(node_proto::ReconcileCephOsdsRequest {
                osd_devices: devices,
                force_wipe: spec.force_wipe,
                encrypt_osds: spec.encrypt_osds,
            }))
            .await
            .map_err(|e| e.to_string())?
            .into_inner();
        if !osd.created.is_empty() {
            hold_backfill(&mut admin, &row.name, &osd.created).await;
        }
        if !osd.success || !osd.pending_retire.is_empty() {
            return Err(format!(
                "{NOT_HEALTHY_YET} waiting for OSD drain: {}",
                osd.message
            ));
        }
        apply_osd_crush_overrides(&mut admin, node, &osd.created).await;
    }

    // Members still in the spec are up, so `ceph mon rm` has a quorum to talk
    // to. Doing this first made removal depend on a node that had not joined.
    retire_removed_members(db, clients, row, &spec).await?;

    let first = spec
        .nodes
        .first()
        .ok_or_else(|| "cluster has no nodes".to_string())?;
    let mut admin = admin_for(db, clients, &first.node_id).await?;

    let default_rule = ceph_cluster_spec::default_crush_rule(&spec);
    let domain = ceph_cluster_spec::failure_domain(&spec);
    if domain != "host" || spec.stretch {
        let tiebreaker = spec
            .nodes
            .iter()
            .find(|n| {
                ceph_cluster_spec::normalize_role(&n.role) == "mon"
                    && ceph_cluster_spec::osd_devices(n).is_empty()
            })
            .map(|n| sanitize_daemon_id(&n.node_id, 0))
            .unwrap_or_default();
        let topo = admin
            .ensure_ceph_crush_topology(Request::new(node_proto::EnsureCephCrushTopologyRequest {
                failure_domain: domain.to_string(),
                hosts: spec
                    .nodes
                    .iter()
                    .map(|n| node_proto::CephCrushHostPlacement {
                        host: sanitize_daemon_id(&n.node_id, 0),
                        rack: n.rack.clone(),
                        room: n.room.clone(),
                    })
                    .collect(),
                stretch: spec.stretch,
                stretch_tiebreaker: tiebreaker,
            }))
            .await
            .map_err(|e| e.to_string())?
            .into_inner();
        if !topo.success {
            return Err(topo.message);
        }
    }

    let mut pools = vec![node_proto::EnsureCephPoolRequest {
        pool: RBD_POOL.into(),
        size: spec.size,
        min_size: spec.min_size,
        application: "rbd".into(),
        pg_num: 0,
        crush_rule: default_rule.clone(),
        compression_mode: String::new(),
        quota_bytes: 0,
        pool_type: "replicated".into(),
        erasure_profile: String::new(),
        ms_mode: spec.ms_mode.clone(),
        recovery: spec.recovery.map(|r| node_proto::CephRecoverySpec {
            max_backfills: r.max_backfills,
            recovery_max_active: r.recovery_max_active,
            scrub_begin_hour: r.scrub_begin_hour,
            scrub_end_hour: r.scrub_end_hour,
            nearfull_ratio_bp: r.nearfull_ratio_bp,
        }),
    }];
    for p in &spec.pools {
        if p.name.trim().is_empty() || p.name.trim() == RBD_POOL {
            continue;
        }
        let crush = if p.crush_rule.trim().is_empty() {
            default_rule.clone()
        } else {
            p.crush_rule.clone()
        };
        pools.push(node_proto::EnsureCephPoolRequest {
            pool: p.name.clone(),
            size: if p.size > 0 { p.size } else { spec.size },
            min_size: if p.min_size > 0 {
                p.min_size
            } else {
                spec.min_size
            },
            application: p.application.clone(),
            pg_num: p.pg_num,
            crush_rule: crush,
            compression_mode: p.compression_mode.clone(),
            quota_bytes: p.quota_bytes,
            pool_type: p.pool_type.clone(),
            erasure_profile: p.erasure_profile.clone(),
            ms_mode: String::new(),
            recovery: None,
        });
    }
    for pool_req in pools {
        let pool = admin
            .ensure_ceph_pool(Request::new(pool_req))
            .await
            .map_err(|e| e.to_string())?
            .into_inner();
        if !pool.success {
            return Err(pool.message);
        }
    }
    // Same key on every member: each writes ceph.client.kcore.keyring so rbd
    // map/create there can run as client.kcore.
    for node in &spec.nodes {
        let result = match admin_for(db, clients, &node.node_id).await {
            Ok(mut member) => member
                .ensure_ceph_client_key(Request::new(node_proto::EnsureCephClientKeyRequest {
                    client_name: "client.kcore".into(),
                    pool: RBD_POOL.into(),
                }))
                .await
                .map(|_| ())
                .map_err(|e| e.to_string()),
            Err(e) => Err(e),
        };
        if let Err(error) = result {
            warn!(node = %node.node_id, %error, "client.kcore key not ensured");
        }
    }

    if let Err(error) = admin
        .enable_ceph_mgr_module(Request::new(node_proto::EnableCephMgrModuleRequest {
            module: "prometheus".into(),
        }))
        .await
    {
        warn!(cluster = %row.name, %error, "mgr prometheus module not enabled");
    }
    if let Err(error) = admin
        .enable_ceph_mgr_module(Request::new(node_proto::EnableCephMgrModuleRequest {
            module: "dashboard".into(),
        }))
        .await
    {
        warn!(cluster = %row.name, %error, "mgr dashboard module not enabled");
    }
    if let Ok(Some(creds)) = db.get_ceph_dashboard_credentials(&row.name) {
        if !creds.username.trim().is_empty() && !creds.password.is_empty() {
            match admin
                .configure_ceph_dashboard(Request::new(node_proto::ConfigureCephDashboardRequest {
                    username: creds.username.clone(),
                    password: creds.password.clone(),
                    server_addr: first.mon_addr.split(':').next().unwrap_or("").to_string(),
                    server_port: 8443,
                }))
                .await
            {
                Ok(resp) => {
                    let inner = resp.into_inner();
                    if !inner.url.is_empty() {
                        let _ = db.upsert_ceph_dashboard_credentials(
                            &crate::db::CephDashboardCredentialsRow {
                                name: row.name.clone(),
                                url: inner.url,
                                username: creds.username.clone(),
                                password: creds.password.clone(),
                                updated_at: String::new(),
                            },
                        );
                    }
                }
                Err(error) => {
                    warn!(cluster = %row.name, %error, "ceph dashboard configure failed");
                }
            }
        }
    }

    if let Some(mirroring) = &spec.mirroring {
        if mirroring.enabled {
            let mode = if mirroring.mode.trim().is_empty() {
                "pool"
            } else {
                mirroring.mode.trim()
            };
            if let Err(error) = admin
                .ensure_rbd_mirror(Request::new(node_proto::EnsureRbdMirrorRequest {
                    pool: RBD_POOL.into(),
                    mode: mode.into(),
                }))
                .await
            {
                warn!(cluster = %row.name, %error, "rbd-mirror enable failed");
            } else if !mirroring.peer_cluster.trim().is_empty() {
                if let Err(error) =
                    ensure_rbd_mirror_peer(db, clients, &row.name, &spec, mirroring).await
                {
                    warn!(cluster = %row.name, %error, "rbd-mirror peer exchange failed");
                }
            }
        }
    }

    let mut health = admin
        .get_ceph_health(Request::new(node_proto::GetCephHealthRequest {
            cluster_name: CEPH_CLUSTER_NAME.into(),
        }))
        .await
        .map_err(|e| e.to_string())?
        .into_inner();
    let ramp_note = settle_backfill_ramp(&mut admin, &row.name, &mut health).await;
    let phase = phase_from_health(&health.health_status);
    let observed = if phase == "healthy" {
        row.generation
    } else {
        // Stay queued for degraded/failed until HEALTH_OK.
        0
    };
    let flags = if health.flags.is_empty() {
        String::new()
    } else {
        format!(" flags={}", health.flags.join(","))
    };
    let prev = db
        .get_ceph_cluster_status(&row.name)
        .ok()
        .flatten()
        .map(|s| s.health_message);
    let health_message = format!(
        "{} (osd {}/{}/{}, pgs {} clean-wait {}{}){}",
        health.health_status,
        health.osd_up,
        health.osd_in,
        health.osd_total,
        health.pgs_total,
        health.pgs_not_clean,
        flags,
        ramp_note
    );
    db.upsert_ceph_cluster_status(&CephClusterStatusRow {
        name: row.name.clone(),
        observed_generation: observed,
        phase: phase.into(),
        health_message: health_message.clone(),
        ceph_status_json: health.raw_status.clone(),
        last_transition_at: String::new(),
    })
    .map_err(|e| e.to_string())?;
    maybe_emit_ceph_health_webhook(
        &row.name,
        prev.as_deref(),
        &health.health_status,
        &health_message,
    );
    if phase != "healthy" {
        return Err(format!(
            "{NOT_HEALTHY_YET} {} (osd up {}, in {})",
            health.health_status, health.osd_up, health.osd_in
        ));
    }
    Ok(())
}

/// Apply optional `weight` / `deviceClass` from the node spec to OSDs that
/// were just created. Best effort: failures only warn.
async fn apply_osd_crush_overrides(
    admin: &mut crate::node_proto::node_admin_client::NodeAdminClient<
        crate::node_client::NodeChannel,
    >,
    node: &crate::controller_proto::CephClusterNodeSpec,
    created: &[String],
) {
    let weight = node.weight;
    let class = node.device_class.trim();
    if weight <= 0.0 && class.is_empty() {
        return;
    }
    for osd_id in created {
        if weight > 0.0 {
            if let Err(error) = admin
                .reweight_ceph_osd(Request::new(node_proto::ReweightCephOsdRequest {
                    osd_id: osd_id.clone(),
                    weight,
                }))
                .await
            {
                warn!(node = %node.node_id, %osd_id, %error, "crush reweight failed");
            }
        }
        if !class.is_empty() {
            if let Err(error) = admin
                .set_ceph_osd_device_class(Request::new(node_proto::SetCephOsdDeviceClassRequest {
                    osd_id: osd_id.clone(),
                    device_class: class.to_string(),
                }))
                .await
            {
                warn!(node = %node.node_id, %osd_id, %error, "set-device-class failed");
            }
        }
    }
}

/// New OSDs make CRUSH remap PGs onto them. Hold `norebalance` so backfill
/// does not start before the recovery throttles from the spec are applied.
/// Best effort: a failure here only means backfill starts at full speed.
async fn hold_backfill(
    admin: &mut crate::node_proto::node_admin_client::NodeAdminClient<
        crate::node_client::NodeChannel,
    >,
    cluster: &str,
    created: &[String],
) {
    match admin
        .set_ceph_flags(Request::new(node_proto::SetCephFlagsRequest {
            set_flags: vec!["norebalance".into()],
            unset_flags: vec![],
        }))
        .await
    {
        Ok(_) => {
            backfill_ramp()
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .insert(cluster.to_string());
        }
        Err(error) => {
            warn!(%cluster, ?created, %error, "could not set norebalance after creating OSDs");
        }
    }
}

/// The ramp is released once every OSD is up and in and no PG is in an
/// unhealthy-for-data state. It cannot wait for HEALTH_OK: the flag itself
/// raises OSDMAP_FLAGS, and remapped PGs stay not-clean while it is set.
pub(crate) fn backfill_ramp_ready(h: &node_proto::GetCephHealthResponse) -> bool {
    h.health_status != "HEALTH_ERR"
        && h.osd_total > 0
        && h.osd_up == h.osd_total
        && h.osd_in == h.osd_total
        && h.backfill_pgs == 0
}

/// Release or keep the ramp and return the text to append to `health_message`.
async fn settle_backfill_ramp(
    admin: &mut crate::node_proto::node_admin_client::NodeAdminClient<
        crate::node_client::NodeChannel,
    >,
    cluster: &str,
    health: &mut node_proto::GetCephHealthResponse,
) -> String {
    let active = backfill_ramp()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .contains(cluster);
    if !active {
        return String::new();
    }
    if !health.norebalance {
        // Cleared by someone else; nothing left to release.
        backfill_ramp()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(cluster);
        return String::new();
    }
    if !backfill_ramp_ready(health) {
        return format!(
            " {BACKFILL_RAMP_NOTE} norebalance held until all OSDs are up and in ({}/{}/{})",
            health.osd_up, health.osd_in, health.osd_total
        );
    }
    let released = admin
        .set_ceph_flags(Request::new(node_proto::SetCephFlagsRequest {
            set_flags: vec![],
            unset_flags: vec!["norebalance".into()],
        }))
        .await;
    if let Err(error) = released {
        warn!(%cluster, %error, "could not clear norebalance");
        return format!(" {BACKFILL_RAMP_NOTE} norebalance still held (clear failed: {error})");
    }
    backfill_ramp()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .remove(cluster);
    if let Ok(fresh) = admin
        .get_ceph_health(Request::new(node_proto::GetCephHealthRequest {
            cluster_name: CEPH_CLUSTER_NAME.into(),
        }))
        .await
    {
        *health = fresh.into_inner();
    }
    " backfill ramp released: norebalance cleared, recovery runs under the spec throttles".into()
}

async fn retire_removed_members(
    db: &Database,
    clients: &NodeClients,
    row: &CephClusterRow,
    spec: &crate::controller_proto::CephClusterSpec,
) -> Result<(), String> {
    let retired = db
        .list_retired_ceph_nodes(&row.name)
        .map_err(|e| e.to_string())?;
    for node_id in retired {
        if spec.nodes.iter().any(|n| n.node_id == node_id) {
            db.clear_retired_ceph_node(&row.name, &node_id)
                .map_err(|e| e.to_string())?;
            continue;
        }
        drain_member(db, clients, row, spec, &node_id).await?;
        let lost = spec.lost_members.iter().any(|m| m.trim() == node_id.trim());
        if lost {
            let Some(survivor) = spec.nodes.first() else {
                return Ok(());
            };
            let host = db
                .get_node(&node_id)
                .map_err(|e| e.to_string())?
                .map(|n| n.hostname)
                .filter(|h| !h.trim().is_empty())
                .unwrap_or_else(|| node_id.clone());
            let daemon_id = sanitize_daemon_id(&node_id, 0);
            let mut survivor_admin = admin_for(db, clients, &survivor.node_id).await?;
            let purged = survivor_admin
                .purge_lost_ceph_host(Request::new(node_proto::PurgeLostCephHostRequest {
                    host,
                    daemon_id,
                }))
                .await
                .map_err(|e| format!("purge lost {node_id} via {}: {e}", survivor.node_id))?
                .into_inner();
            if !purged.success {
                return Err(format!("purge lost {node_id}: {}", purged.message));
            }
        } else {
            let mut admin = admin_for(db, clients, &node_id).await?;
            let torn = admin
                .teardown_ceph_node(Request::new(node_proto::TeardownCephNodeRequest {}))
                .await
                .map_err(|e| format!("teardown {node_id}: {e}"))?
                .into_inner();
            if !torn.success {
                return Err(format!("teardown {node_id}: {}", torn.message));
            }
            if !torn.apply_id.is_empty() {
                wait_for_nix_apply(&mut admin, &torn.apply_id).await?;
            }
            if !torn.daemon_id.is_empty() {
                if let Some(survivor) = spec.nodes.first() {
                    let mut survivor_admin = admin_for(db, clients, &survivor.node_id).await?;
                    let removed = survivor_admin
                        .remove_ceph_mon(Request::new(node_proto::RemoveCephMonRequest {
                            daemon_id: torn.daemon_id.clone(),
                        }))
                        .await
                        .map_err(|e| {
                            format!(
                                "ceph mon rm {} via {}: {e}",
                                torn.daemon_id, survivor.node_id
                            )
                        })?
                        .into_inner();
                    if !removed.success {
                        return Err(format!(
                            "ceph mon rm {} via {}: {}",
                            torn.daemon_id, survivor.node_id, removed.message
                        ));
                    }
                }
            }
        }
        db.clear_retired_ceph_node(&row.name, &node_id)
            .map_err(|e| e.to_string())?;
    }
    Ok(())
}

/// Mark the leaving member's OSDs out from a survivor and hold the removal
/// until every PG is active+clean again. Returns the `NOT_HEALTHY_YET`
/// error (after recording the wait in the cluster status) while data is
/// still moving, so the next tick retries instead of tearing the host down
/// with the only copy of something still on it.
async fn drain_member(
    db: &Database,
    clients: &NodeClients,
    row: &CephClusterRow,
    spec: &crate::controller_proto::CephClusterSpec,
    node_id: &str,
) -> Result<(), String> {
    let Some(survivor) = spec.nodes.first() else {
        // Nothing survives: DeleteCephCluster, not a member removal.
        return Ok(());
    };
    let host = db
        .get_node(node_id)
        .map_err(|e| e.to_string())?
        .map(|n| n.hostname)
        .filter(|h| !h.trim().is_empty())
        .unwrap_or_else(|| node_id.to_string());
    let mut survivor_admin = admin_for(db, clients, &survivor.node_id).await?;
    let drained = survivor_admin
        .drain_ceph_host(Request::new(node_proto::DrainCephHostRequest {
            host: host.clone(),
        }))
        .await
        .map_err(|e| {
            if e.code() == tonic::Code::Unimplemented {
                format!(
                    "survivor {} runs a node agent without DrainCephHost; update it before removing {node_id}",
                    survivor.node_id
                )
            } else {
                format!("drain {host} via {}: {e}", survivor.node_id)
            }
        })?
        .into_inner();
    if !drained.success {
        return Err(format!("drain {host}: {}", drained.message));
    }
    if drained.clean {
        return Ok(());
    }
    let message = format!(
        "removing {node_id}: {} ({} PGs of {} still not active+clean)",
        drained.message, drained.pgs_not_clean, drained.pgs_total
    );
    db.upsert_ceph_cluster_status(&CephClusterStatusRow {
        name: row.name.clone(),
        observed_generation: 0,
        phase: "degraded".into(),
        health_message: message.clone(),
        ceph_status_json: String::new(),
        last_transition_at: String::new(),
    })
    .map_err(|e| e.to_string())?;
    Err(format!("{NOT_HEALTHY_YET} {message}"))
}

async fn wait_for_nix_apply(
    admin: &mut crate::node_proto::node_admin_client::NodeAdminClient<
        crate::node_client::NodeChannel,
    >,
    apply_id: &str,
) -> Result<(), String> {
    let deadline = tokio::time::Instant::now() + NIX_APPLY_WAIT;
    loop {
        match admin
            .get_nix_apply_status(Request::new(node_proto::GetNixApplyStatusRequest {
                apply_id: apply_id.to_string(),
            }))
            .await
        {
            Ok(resp) => {
                let resp = resp.into_inner();
                match resp.phase {
                    p if p == node_proto::NixApplyPhase::Succeeded as i32 => return Ok(()),
                    p if p == node_proto::NixApplyPhase::Failed as i32 => {
                        return Err(format!("nixos-rebuild {apply_id} failed: {}", resp.message));
                    }
                    p if p == node_proto::NixApplyPhase::Unknown as i32 => {
                        warn!(%apply_id, message = %resp.message, "no nix apply verdict; continuing");
                        return Ok(());
                    }
                    _ => {}
                }
            }
            Err(e) if e.code() == tonic::Code::Unavailable => {}
            Err(e) if e.code() == tonic::Code::Unimplemented => return Ok(()),
            Err(e) => return Err(format!("polling nix apply {apply_id}: {e}")),
        }
        if tokio::time::Instant::now() >= deadline {
            return Err(format!("timed out waiting for nix apply {apply_id}"));
        }
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
}

pub(crate) fn phase_from_health(health_status: &str) -> &'static str {
    match health_status {
        "HEALTH_OK" => "healthy",
        "HEALTH_WARN" => "degraded",
        _ => "failed",
    }
}

pub(crate) fn sanitize_daemon_id(node_id: &str, index: usize) -> String {
    let id: String = node_id
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' {
                c
            } else {
                '-'
            }
        })
        .collect();
    if id.is_empty() {
        format!("node-{index}")
    } else {
        id
    }
}

#[derive(Debug, Clone, Default)]
pub(crate) struct CephNixExtras {
    pub enable_mds: bool,
    pub enable_rgw: bool,
    pub enable_rbd_mirror: bool,
    pub enable_dashboard: bool,
    pub cache_policy: String,
    pub image_features: String,
}

/// MDS/RGW placement from the SharedFilesystems and ObjectStores that
/// reference one cluster.
#[derive(Debug, Default)]
pub(crate) struct CephServiceFlags {
    /// SharedFilesystem has no member list: every full/mon member.
    any_mds: bool,
    /// Some ObjectStore left `members` empty: every full/mon member.
    rgw_all: bool,
    rgw_members: Vec<String>,
}

/// `full`, `mon`, or unset roles run a mon and host MDS/RGW by default.
fn role_hosts_default_daemons(role: &str) -> bool {
    !role.trim().eq_ignore_ascii_case("osd")
}

impl CephServiceFlags {
    pub(crate) fn enable_mds_for(&self, _node_id: &str, role: &str) -> bool {
        self.any_mds && role_hosts_default_daemons(role)
    }

    pub(crate) fn enable_rgw_for(&self, node_id: &str, role: &str) -> bool {
        (self.rgw_all && role_hosts_default_daemons(role))
            || self.rgw_members.iter().any(|m| m.trim() == node_id.trim())
    }

    pub(crate) fn extras_for(
        &self,
        spec: &crate::controller_proto::CephClusterSpec,
        node: &crate::controller_proto::CephClusterNodeSpec,
    ) -> CephNixExtras {
        let mirror = spec.mirroring.as_ref().map(|m| m.enabled).unwrap_or(false);
        CephNixExtras {
            enable_mds: self.enable_mds_for(&node.node_id, &node.role),
            enable_rgw: self.enable_rgw_for(&node.node_id, &node.role),
            enable_rbd_mirror: mirror && role_hosts_default_daemons(&node.role),
            enable_dashboard: true,
            cache_policy: spec.cache_policy.clone(),
            image_features: spec.image_features.clone(),
        }
    }
}

async fn ensure_rbd_mirror_peer(
    db: &Database,
    clients: &NodeClients,
    local_name: &str,
    local_spec: &crate::controller_proto::CephClusterSpec,
    mirroring: &crate::controller_proto::CephMirroringSpec,
) -> Result<(), String> {
    let peer_name = mirroring.peer_cluster.trim();
    let peer_row = db
        .get_ceph_cluster(peer_name)
        .map_err(|e| e.to_string())?
        .ok_or_else(|| format!("peer cluster {peer_name} not found"))?;
    let peer_spec =
        ceph_cluster_spec::spec_from_json(&peer_row.spec_json).map_err(|e| e.to_string())?;
    let local_first = local_spec
        .nodes
        .first()
        .ok_or_else(|| "local cluster has no nodes".to_string())?;
    let peer_first = peer_spec
        .nodes
        .first()
        .ok_or_else(|| "peer cluster has no nodes".to_string())?;
    let mut local = admin_for(db, clients, &local_first.node_id).await?;
    let mut peer = admin_for(db, clients, &peer_first.node_id).await?;
    let token = local
        .create_rbd_mirror_bootstrap(Request::new(node_proto::CreateRbdMirrorBootstrapRequest {
            pool: RBD_POOL.into(),
            site_name: local_name.to_string(),
        }))
        .await
        .map_err(|e| e.to_string())?
        .into_inner();
    if !token.success || token.token.is_empty() {
        return Err(token.message);
    }
    let imported = peer
        .import_rbd_mirror_peer(Request::new(node_proto::ImportRbdMirrorPeerRequest {
            pool: RBD_POOL.into(),
            token: token.token,
            site_name: peer_name.to_string(),
        }))
        .await
        .map_err(|e| e.to_string())?
        .into_inner();
    if !imported.success {
        return Err(imported.message);
    }
    Ok(())
}

pub(crate) fn ceph_service_flags(
    db: &Database,
    cluster_name: &str,
) -> Result<CephServiceFlags, String> {
    let mut flags = CephServiceFlags::default();
    for fs in db.list_shared_filesystems().map_err(|e| e.to_string())? {
        let Ok(spec) = crate::shared_filesystem_spec::spec_from_json(&fs.spec_json) else {
            continue;
        };
        if spec.ceph_cluster.trim() == cluster_name {
            flags.any_mds = true;
            break;
        }
    }
    for store in db.list_object_stores().map_err(|e| e.to_string())? {
        let Ok(spec) = crate::object_store_spec::spec_from_json(&store.spec_json) else {
            continue;
        };
        if spec.ceph_cluster.trim() != cluster_name {
            continue;
        }
        let members: Vec<String> = spec
            .members
            .into_iter()
            .filter(|m| !m.trim().is_empty())
            .collect();
        if members.is_empty() {
            flags.rgw_all = true;
        } else {
            flags.rgw_members.extend(members);
        }
    }
    Ok(flags)
}

pub(crate) fn generate_ceph_nix(
    spec: &crate::controller_proto::CephClusterSpec,
    node: &crate::controller_proto::CephClusterNodeSpec,
    daemon_id: &str,
    mon_map: &str,
    extras: &CephNixExtras,
) -> String {
    let mon_host: String = mon_map
        .split(',')
        .filter_map(|p| p.split_once('=').map(|(_, ip)| ip.trim().to_string()))
        .collect::<Vec<_>>()
        .join(",");
    let first_mon = mon_map
        .split(',')
        .next()
        .and_then(|p| p.split_once('=').map(|(id, _)| id.trim()))
        .unwrap_or_default();
    let initial_members = if first_mon == daemon_id {
        daemon_id
    } else {
        ""
    };
    let public_addr = node
        .mon_addr
        .split(':')
        .next()
        .unwrap_or(&node.mon_addr)
        .trim();
    let role = node.role.trim().to_ascii_lowercase();
    let (enable_mon, enable_mgr, enable_osd) = match role.as_str() {
        "mon" => (true, true, false),
        "osd" => (false, false, true),
        _ => (true, true, true),
    };
    let cache = extras.cache_policy.trim();
    let features = extras.image_features.trim();
    let env_lines = {
        let mut lines = String::new();
        if !cache.is_empty() {
            lines.push_str(&format!(
                "\n    environment.variables.KCORE_RBD_CACHE_POLICY = \"{}\";",
                nix_escape(cache)
            ));
        }
        if !features.is_empty() {
            lines.push_str(&format!(
                "\n    environment.variables.KCORE_RBD_IMAGE_FEATURES = \"{}\";",
                nix_escape(features)
            ));
        }
        lines
    };
    format!(
        r#"{{ ... }}: {{
  kcore.ceph = {{
    enable = true;
    clusterName = "{cluster}";
    fsid = "{fsid}";
    publicNetwork = "{public_network}";
    clusterNetwork = "{cluster_network}";
    monAddress = "{mon_host}";
    # Only the first mon names itself. Ceph prunes (or pads with 0.0.0.0
    # placeholders) the monmap to match this list on first start, so a mon
    # joining a running cluster must leave it empty to keep the adopted map.
    monInitialMembers = "{initial_members}";
    publicAddr = "{public_addr}";
    clusterAddr = "{cluster_addr}";
    publicIface = "{public_iface}";
    clusterIface = "{cluster_iface}";
    daemonId = "{daemon_id}";
    enableMon = {enable_mon};
    enableMgr = {enable_mgr};
    enableOsd = {enable_osd};
    enableMds = {enable_mds};
    enableRgw = {enable_rgw};
    enableRbdMirror = {enable_rbd_mirror};
    enableDashboard = {enable_dashboard};
    poolSize = {size};
    poolMinSize = {min_size};
  }};{env_lines}
}}
"#,
        cluster = nix_escape(CEPH_CLUSTER_NAME),
        fsid = nix_escape(&spec.fsid),
        public_network = nix_escape(&spec.public_network),
        cluster_network = nix_escape(&spec.cluster_network),
        cluster_addr = nix_escape(&node.cluster_addr),
        public_iface = nix_escape(&node.public_iface),
        cluster_iface = nix_escape(&node.cluster_iface),
        size = spec.size,
        min_size = spec.min_size,
        mon_host = nix_escape(&mon_host),
        initial_members = nix_escape(initial_members),
        public_addr = nix_escape(public_addr),
        daemon_id = nix_escape(daemon_id),
        enable_mon = if enable_mon { "true" } else { "false" },
        enable_mgr = if enable_mgr { "true" } else { "false" },
        enable_osd = if enable_osd { "true" } else { "false" },
        enable_mds = if extras.enable_mds { "true" } else { "false" },
        enable_rgw = if extras.enable_rgw { "true" } else { "false" },
        enable_rbd_mirror = if extras.enable_rbd_mirror {
            "true"
        } else {
            "false"
        },
        enable_dashboard = if extras.enable_dashboard {
            "true"
        } else {
            "false"
        },
        env_lines = env_lines,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::controller_proto::{CephClusterNodeSpec, CephClusterSpec};

    fn sample_spec() -> CephClusterSpec {
        CephClusterSpec {
            fsid: "fsid-1".into(),
            public_network: "10.10.0.0/24".into(),
            cluster_network: "10.20.0.0/24".into(),
            size: 3,
            min_size: 2,
            force_wipe: false,
            encrypt_osds: true,
            nodes: vec![
                CephClusterNodeSpec {
                    node_id: "dell.tower/1".into(),
                    mon_addr: "10.10.0.11:6789".into(),
                    cluster_addr: "10.20.0.11".into(),
                    public_iface: "eth1".into(),
                    cluster_iface: "eth2".into(),
                    osd_device: "/dev/nvme0n1".into(),
                    osd_devices: vec![],
                    ..Default::default()
                },
                CephClusterNodeSpec {
                    node_id: "dell-2".into(),
                    mon_addr: "10.10.0.12:6789".into(),
                    cluster_addr: "10.20.0.12".into(),
                    public_iface: "eth1".into(),
                    cluster_iface: "eth2".into(),
                    osd_device: "/dev/nvme0n1".into(),
                    osd_devices: vec![],
                    ..Default::default()
                },
            ],
            ..Default::default()
        }
    }

    #[test]
    fn phase_from_health_maps_ceph_status_strings() {
        assert_eq!(phase_from_health("HEALTH_OK"), "healthy");
        assert_eq!(phase_from_health("HEALTH_WARN"), "degraded");
        assert_eq!(phase_from_health("HEALTH_ERR"), "failed");
        assert_eq!(phase_from_health(""), "failed");
        assert_eq!(phase_from_health("unknown"), "failed");
    }

    #[test]
    fn sanitize_daemon_id_keeps_safe_chars_and_replaces_rest() {
        assert_eq!(sanitize_daemon_id("dell-1", 0), "dell-1");
        assert_eq!(sanitize_daemon_id("dell.tower/1", 0), "dell-tower-1");
        assert_eq!(sanitize_daemon_id("@@@", 2), "---");
        assert_eq!(sanitize_daemon_id("", 3), "node-3");
    }

    #[test]
    fn generate_ceph_nix_uses_fixed_cluster_name_and_all_mons() {
        let spec = sample_spec();
        let mon_map = mon_map_string(&spec);
        assert!(mon_map.contains("dell-tower-1=10.10.0.11"));
        assert!(mon_map.contains("dell-2=10.10.0.12"));
        let nix = generate_ceph_nix(
            &spec,
            &spec.nodes[0],
            "dell-tower-1",
            &mon_map,
            &CephNixExtras::default(),
        );
        assert!(nix.contains("clusterName = \"ceph\";"));
        assert!(!nix.contains("clusterName = \"lab\""));
        assert!(nix.contains("monAddress = \"10.10.0.11,10.10.0.12\";"));
        // Local daemon only — peers join via ceph mon add, not initial-members.
        assert!(nix.contains("monInitialMembers = \"dell-tower-1\";"));
        assert!(
            !nix.lines().any(|l| l.trim_start().starts_with("//")),
            "Nix comments use #; // is the update operator"
        );
        assert!(!nix.contains("monInitialMembers = \"dell-tower-1,dell-2\";"));
        assert!(nix.contains("publicAddr = \"10.10.0.11\";"));
        assert!(nix.contains("clusterAddr = \"10.20.0.11\";"));
        assert!(nix.contains("publicIface = \"eth1\";"));
        assert!(nix.contains("clusterIface = \"eth2\";"));
        assert!(nix.contains("daemonId = \"dell-tower-1\";"));
        assert!(nix.contains("poolSize = 3;"));
        assert!(nix.contains("enableMds = false;"));
        assert!(nix.contains("enableRgw = false;"));
    }

    #[test]
    fn generate_ceph_nix_roles_and_mds_rgw() {
        let mut spec = sample_spec();
        spec.nodes[0].role = "mon".into();
        spec.nodes[1].role = "osd".into();
        let mon_map = mon_map_string(&spec);
        let mon_nix = generate_ceph_nix(
            &spec,
            &spec.nodes[0],
            "dell-tower-1",
            &mon_map,
            &CephNixExtras {
                enable_mds: true,
                enable_rgw: false,
                enable_rbd_mirror: false,
                enable_dashboard: true,
                cache_policy: "writethrough".into(),
                image_features: "layering,exclusive-lock".into(),
            },
        );
        assert!(mon_nix.contains("enableMon = true;"));
        assert!(mon_nix.contains("enableMgr = true;"));
        assert!(mon_nix.contains("enableOsd = false;"));
        assert!(mon_nix.contains("enableMds = true;"));
        assert!(mon_nix.contains("KCORE_RBD_CACHE_POLICY = \"writethrough\";"));
        let osd_nix = generate_ceph_nix(
            &spec,
            &spec.nodes[1],
            "dell-2",
            &mon_map,
            &CephNixExtras {
                enable_mds: false,
                enable_rgw: true,
                ..Default::default()
            },
        );
        assert!(osd_nix.contains("enableMon = false;"));
        assert!(osd_nix.contains("enableOsd = true;"));
        assert!(osd_nix.contains("enableRgw = true;"));
    }

    #[test]
    fn joining_mon_leaves_initial_members_empty() {
        let spec = sample_spec();
        let mon_map = mon_map_string(&spec);
        let nix = generate_ceph_nix(
            &spec,
            &spec.nodes[1],
            "dell-2",
            &mon_map,
            &CephNixExtras::default(),
        );
        assert!(nix.contains("monInitialMembers = \"\";"));
        assert!(nix.contains("daemonId = \"dell-2\";"));
    }

    #[test]
    fn service_flags_default_to_full_and_mon_members() {
        let flags = CephServiceFlags {
            any_mds: true,
            rgw_all: true,
            rgw_members: vec![],
        };
        for role in ["", "full", "mon", "MON"] {
            assert!(flags.enable_mds_for("n1", role), "{role}");
            assert!(flags.enable_rgw_for("n1", role), "{role}");
        }
        assert!(!flags.enable_mds_for("n1", "osd"));
        assert!(!flags.enable_rgw_for("n1", "osd"));
    }

    #[test]
    fn rgw_members_pick_named_nodes_only() {
        let flags = CephServiceFlags {
            any_mds: false,
            rgw_all: false,
            rgw_members: vec!["n2".into()],
        };
        assert!(!flags.enable_mds_for("n2", "full"));
        assert!(flags.enable_rgw_for("n2", "full"));
        assert!(flags.enable_rgw_for("n2", "osd"));
        assert!(!flags.enable_rgw_for("n1", "full"));
    }

    #[test]
    fn extras_for_carries_flags_into_nix() {
        let mut spec = sample_spec();
        spec.nodes[1].role = "osd".into();
        let flags = CephServiceFlags {
            any_mds: true,
            rgw_all: false,
            rgw_members: vec!["dell-2".into()],
        };
        let mon_map = mon_map_string(&spec);
        let first = generate_ceph_nix(
            &spec,
            &spec.nodes[0],
            "dell-tower-1",
            &mon_map,
            &flags.extras_for(&spec, &spec.nodes[0]),
        );
        assert!(first.contains("enableMds = true;"));
        assert!(first.contains("enableRgw = false;"));
        let second = generate_ceph_nix(
            &spec,
            &spec.nodes[1],
            "dell-2",
            &mon_map,
            &flags.extras_for(&spec, &spec.nodes[1]),
        );
        assert!(second.contains("enableMds = false;"));
        assert!(second.contains("enableRgw = true;"));
    }

    #[test]
    fn backfill_ramp_releases_when_osds_are_up_and_in() {
        let mut h = node_proto::GetCephHealthResponse {
            health_status: "HEALTH_WARN".into(),
            osd_total: 4,
            osd_up: 3,
            osd_in: 3,
            norebalance: true,
            ..Default::default()
        };
        assert!(!backfill_ramp_ready(&h));
        h.osd_up = 4;
        h.osd_in = 4;
        assert!(backfill_ramp_ready(&h));
        h.backfill_pgs = 2;
        assert!(!backfill_ramp_ready(&h));
        h.backfill_pgs = 0;
        h.health_status = "HEALTH_ERR".into();
        assert!(!backfill_ramp_ready(&h));
    }

    #[test]
    fn daemon_push_note_reports_targets_and_apply_state() {
        let db = Database::open(":memory:").expect("db");
        let spec = sample_spec();
        db.upsert_ceph_cluster(&CephClusterRow {
            name: "c1".into(),
            generation: 1,
            spec_json: ceph_cluster_spec::spec_to_json(&spec).expect("json"),
            bootstrap_json: String::new(),
            created_at: String::new(),
            updated_at: String::new(),
        })
        .expect("cluster");
        let note = daemon_push_note(&db, "c1", DaemonKind::Mds).expect("note");
        assert!(note.contains("no member"), "{note}");

        let fs_spec = crate::controller_proto::SharedFilesystemSpec {
            ceph_cluster: "c1".into(),
            ..Default::default()
        };
        db.upsert_shared_filesystem(&crate::db::SharedFilesystemRow {
            name: "fs1".into(),
            generation: 1,
            spec_json: crate::shared_filesystem_spec::spec_to_json(&fs_spec).expect("json"),
            created_at: String::new(),
            updated_at: String::new(),
        })
        .expect("fs");
        let queued = daemon_push_note(&db, "c1", DaemonKind::Mds).expect("note");
        assert!(
            queued.contains("queued for dell.tower/1,dell-2"),
            "{queued}"
        );

        db.upsert_ceph_cluster_status(&CephClusterStatusRow {
            name: "c1".into(),
            observed_generation: 1,
            phase: "healthy".into(),
            health_message: String::new(),
            ceph_status_json: String::new(),
            last_transition_at: String::new(),
        })
        .expect("status");
        let pushed = daemon_push_note(&db, "c1", DaemonKind::Mds).expect("note");
        assert!(
            pushed.contains("enabled on dell.tower/1,dell-2"),
            "{pushed}"
        );

        db.requeue_ceph_cluster("c1").expect("requeue");
        let again = daemon_push_note(&db, "c1", DaemonKind::Mds).expect("note");
        assert!(again.contains("queued"), "{again}");
    }
}
