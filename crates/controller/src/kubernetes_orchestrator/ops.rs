//! Day-two work the 30s loop does for a `ready` cluster: certificate renewal,
//! drift reconcile, scheduled etcd snapshots, and control-plane repair.
//!
//! Each step returns an optional notice for the cluster status message. A
//! failing step is logged and never stops the others.

use std::path::Path;

use time::format_description::well_known::Rfc3339;
use time::OffsetDateTime;

use super::*;
use crate::db::KubernetesEtcdSnapshotRow;
use crate::kubernetes_backup::{self, BackupSpec, BackupTarget, SnapshotAge};
use crate::kubernetes_cert_renewal::{
    next_renewal_step, renewal_due, KubernetesCertConfig, RenewalMember, RenewalStep, RenewalView,
};
use crate::kubernetes_drift;
use crate::kubernetes_repair::{self, CpHealth, RepairDecision};

/// Bump when `modules/kcore-kubernetes.nix` changes in a way the rendered Nix
/// alone does not show, so every node is reconciled onto it.
pub const MODULE_VERSION: &str = "kcore-kubernetes-module-1";
pub const DEFAULT_BACKUP_ROOT: &str = "/var/lib/kcore/kubernetes-backups";

pub const NOTICE_DRIFT: &str = "configuration drift";
pub const NOTICE_REPAIR: &str = "repair alert";
pub const NOTICE_BACKUP: &str = "backup failed";
pub const NOTICE_CERTS: &str = "certificate renewal";
pub const NOTICE_CA: &str = "CA expiry";
const NOTICE_PREFIXES: [&str; 5] = [
    NOTICE_DRIFT,
    NOTICE_REPAIR,
    NOTICE_BACKUP,
    NOTICE_CERTS,
    NOTICE_CA,
];

pub fn is_notice(message: &str) -> bool {
    NOTICE_PREFIXES
        .iter()
        .any(|prefix| message.starts_with(prefix))
}

enum DriftOutcome {
    Clean,
    /// Phase moved to reconfiguring; stop for this tick.
    Reconciling,
    Pending(String),
}

fn unix_now() -> i64 {
    OffsetDateTime::now_utc().unix_timestamp()
}

fn parse_unix(rfc3339: &str) -> Option<i64> {
    OffsetDateTime::parse(rfc3339, &Rfc3339)
        .ok()
        .map(|t| t.unix_timestamp())
}

/// Units a member restarts onto renewed certificates. The primary cluster
/// (API port 6443) has bare names; every other cluster suffixes its name.
pub fn certificate_units(
    cluster_name: &str,
    api_endpoint: &str,
    control_plane: bool,
) -> Vec<String> {
    let suffix = if crate::nixgen::kubernetes_listen_ports(api_endpoint).api_server == 6443 {
        String::new()
    } else {
        format!("-{cluster_name}")
    };
    let names: &[&str] = if control_plane {
        &[
            "etcd",
            "kube-apiserver",
            "kube-controller-manager",
            "kube-scheduler",
            "kubelet",
        ]
    } else {
        &["kubelet"]
    };
    names.iter().map(|name| format!("{name}{suffix}")).collect()
}

impl KubernetesOrchestrator {
    /// Everything a ready cluster needs on a tick. Held under the cluster
    /// claim, so it never overlaps a scale, upgrade, or rollout.
    pub(super) async fn reconcile_ready(&self, cluster_name: &str) {
        let mut notices: Vec<String> = Vec::new();

        match self.record_etcd_health(cluster_name).await {
            Ok(()) => {}
            Err(e) => info!(cluster = cluster_name, error = %e, "etcd health sample failed"),
        }

        match self.reconcile_cert_renewal(cluster_name, false).await {
            Ok(Some(notice)) => notices.push(notice),
            Ok(None) => {}
            Err(e) => {
                error!(cluster = cluster_name, error = %format!("{e:#}"), "certificate renewal failed");
                notices.push(format!("{NOTICE_CERTS} failed: {e:#}"));
            }
        }

        match self.reconcile_drift(cluster_name).await {
            Ok(DriftOutcome::Clean) => {}
            Ok(DriftOutcome::Reconciling) => return,
            Ok(DriftOutcome::Pending(notice)) => notices.push(notice),
            Err(e) => {
                error!(cluster = cluster_name, error = %format!("{e:#}"), "drift check failed");
            }
        }

        match self.reconcile_backup(cluster_name).await {
            Ok(Some(notice)) => notices.push(notice),
            Ok(None) => {}
            Err(e) => {
                error!(cluster = cluster_name, error = %format!("{e:#}"), "scheduled backup failed");
                notices.push(format!("{NOTICE_BACKUP}: {e:#}"));
            }
        }

        match self.reconcile_repair(cluster_name).await {
            Ok(Some(notice)) => notices.push(notice),
            Ok(None) => {}
            Err(e) => {
                error!(cluster = cluster_name, error = %format!("{e:#}"), "repair check failed");
            }
        }

        if let Some(notice) = self.ca_expiry_notice(cluster_name) {
            notices.push(notice);
        }

        if let Err(e) = self.publish_notices(cluster_name, &notices) {
            warn!(cluster = cluster_name, error = %e, "could not record cluster notices");
        }
    }

    /// Cluster leaf/CA cert policy from `cert_json`, or defaults.
    pub fn cert_policy(&self, cluster_name: &str) -> Result<KubernetesCertConfig> {
        let ops = self.db.get_kubernetes_cluster_ops(cluster_name)?;
        let policy = ops
            .as_ref()
            .map(|o| crate::kubernetes_hardening::CertPolicy::from_json(&o.cert_json))
            .unwrap_or_default();
        Ok(KubernetesCertConfig {
            leaf_validity_days: policy.leaf_validity_days,
            renew_before_days: policy.renew_before_days,
        })
    }

    fn ca_expiry_notice(&self, cluster_name: &str) -> Option<String> {
        let pki = self.db.load_kubernetes_pki(cluster_name).ok().flatten()?;
        let not_after = kubernetes_pki::pem_not_after_public(&pki.ca_cert)?;
        let days = (not_after - OffsetDateTime::now_utc()).whole_days();
        if days < 365 {
            Some(format!(
                "{NOTICE_CA}: cluster CA expires in {days} days; plan a dual-CA rotation"
            ))
        } else {
            None
        }
    }

    /// Notices go in the status message of a ready cluster, and only
    /// replace messages this module wrote.
    fn publish_notices(&self, cluster_name: &str, notices: &[String]) -> Result<()> {
        let Some(cluster) = self.db.get_kubernetes_cluster(cluster_name)? else {
            return Ok(());
        };
        if cluster.phase != "ready" {
            return Ok(());
        }
        if notices.is_empty() {
            if is_notice(&cluster.status_message) {
                self.db
                    .set_kubernetes_cluster_status(cluster_name, "ready", "cluster ready")?;
            }
            return Ok(());
        }
        let message = notices.join("; ");
        if cluster.status_message != message {
            self.db
                .set_kubernetes_cluster_status(cluster_name, "ready", &message)?;
        }
        Ok(())
    }

    async fn record_etcd_health(&self, cluster_name: &str) -> Result<()> {
        let healthy = self.count_healthy_etcd_members(cluster_name).await?;
        let total = self
            .db
            .list_kubernetes_cluster_nodes(cluster_name)?
            .iter()
            .filter(|row| row.role == "control-plane" && !row.pending_join)
            .count();
        crate::kubernetes_metrics::record_etcd(cluster_name, healthy, total);
        Ok(())
    }

    // ---- drift -----------------------------------------------------------

    pub(super) async fn drift_hash(
        &self,
        cluster_name: &str,
        node_id: &str,
        pki: &KubernetesPki,
    ) -> Result<String> {
        // Rendered without a kubelet kubeconfig on purpose: only joining nodes
        // carry one, and the stored hash is computed the same way.
        let rendered = self.render_node_nix(node_id, cluster_name, pki, "").await?;
        Ok(kubernetes_drift::rendered_hash(
            &rendered.nix,
            MODULE_VERSION,
        ))
    }

    /// Best effort: a failed hash write only delays drift detection.
    pub(super) async fn record_rendered_hash(
        &self,
        cluster_name: &str,
        node_id: &str,
        pki: &KubernetesPki,
    ) {
        let result = async {
            let hash = self.drift_hash(cluster_name, node_id, pki).await?;
            self.db
                .set_kubernetes_node_rendered_hash(cluster_name, node_id, &hash)?;
            anyhow::Ok(())
        }
        .await;
        if let Err(e) = result {
            warn!(cluster = cluster_name, node_id, error = %format!("{e:#}"), "rendered hash not recorded");
        }
    }

    async fn reconcile_drift(&self, cluster_name: &str) -> Result<DriftOutcome> {
        let Some(ops) = self.db.get_kubernetes_cluster_ops(cluster_name)? else {
            return Ok(DriftOutcome::Clean);
        };
        let Some(pki) = self.db.load_kubernetes_pki(cluster_name)? else {
            return Ok(DriftOutcome::Clean);
        };
        let stored = self.db.list_kubernetes_node_rendered_hashes(cluster_name)?;
        let mut drifted = Vec::new();
        for row in self.db.list_kubernetes_cluster_nodes(cluster_name)? {
            if row.pending_join || row.pending_removal {
                continue;
            }
            let current = self.drift_hash(cluster_name, &row.node_id, &pki).await?;
            let previous = stored.get(&row.node_id).map(String::as_str).unwrap_or("");
            if previous.is_empty() {
                // Nothing recorded yet (cluster from before drift tracking):
                // take what renders now as the baseline.
                self.db
                    .set_kubernetes_node_rendered_hash(cluster_name, &row.node_id, &current)?;
            } else if kubernetes_drift::drift_detected(previous, &current) {
                drifted.push(row.node_id);
            }
        }
        if drifted.is_empty() {
            return Ok(DriftOutcome::Clean);
        }
        let message = format!("{NOTICE_DRIFT} on {}", drifted.join(", "));
        if ops.auto_reconcile {
            info!(cluster = cluster_name, nodes = ?drifted, "rendered configuration drifted; reconciling");
            self.db.set_kubernetes_cluster_status(
                cluster_name,
                PHASE_RECONFIGURING,
                &format!("{message}; reconciling"),
            )?;
            return Ok(DriftOutcome::Reconciling);
        }
        Ok(DriftOutcome::Pending(format!(
            "{message}; auto-reconcile is off"
        )))
    }

    // ---- certificates ----------------------------------------------------

    /// Earliest leaf expiry. Clusters stored before it was recorded get it
    /// computed from their certificates once.
    pub fn leaf_expiry(&self, cluster_name: &str) -> Result<Option<OffsetDateTime>> {
        let stored = self.db.kubernetes_pki_leaf_not_after(cluster_name)?;
        if let Ok(parsed) = OffsetDateTime::parse(&stored, &Rfc3339) {
            return Ok(Some(parsed));
        }
        let Some(pki) = self.db.load_kubernetes_pki(cluster_name)? else {
            return Ok(None);
        };
        let Some(not_after) = kubernetes_pki::earliest_leaf_not_after(&pki) else {
            return Ok(None);
        };
        self.db
            .set_kubernetes_pki_leaf_not_after(cluster_name, &not_after.format(&Rfc3339)?)?;
        Ok(Some(not_after))
    }

    /// Issue every leaf again from the stored CAs and store the result.
    pub(super) async fn reissue_leaf_certs(&self, cluster_name: &str) -> Result<()> {
        let cluster = self
            .db
            .get_kubernetes_cluster(cluster_name)?
            .context("cluster not found")?;
        let mut pki = self
            .db
            .load_kubernetes_pki(cluster_name)?
            .context("PKI not found")?;
        let infos: Vec<NodeInfo> = self
            .get_cluster_nodes(cluster_name)
            .await?
            .into_iter()
            .map(|n| NodeInfo {
                hostname: if n.hostname.is_empty() {
                    n.node_id.clone()
                } else {
                    n.hostname
                },
                node_id: n.node_id,
                ip_address: n.ip_address,
            })
            .collect();
        let cfg = self.cert_policy(cluster_name)?;
        kubernetes_pki::renew_leaf_certificates_for(
            &mut pki,
            cluster_name,
            &cluster.api_server_endpoint,
            &cluster.service_cidr,
            &infos,
            cfg.leaf_validity_days.max(1) as u32,
        )?;
        self.db.store_kubernetes_pki(cluster_name, &pki)?;
        Ok(())
    }

    /// Renew when due (or `force`d): re-issue the leaves, then rebuild and
    /// restart one node at a time, control planes first. The new leaves are
    /// stored before the first node moves; a roll cut short is finished by
    /// the drift reconcile, because the nodes' hashes no longer match.
    pub async fn reconcile_cert_renewal(
        &self,
        cluster_name: &str,
        force: bool,
    ) -> Result<Option<String>> {
        let Some(not_after) = self.leaf_expiry(cluster_name)? else {
            return Ok(None);
        };
        let cfg = self.cert_policy(cluster_name)?;
        if !force && !renewal_due(OffsetDateTime::now_utc(), not_after, cfg.renew_before_days) {
            return Ok(None);
        }

        let serving: HashSet<String> = self
            .db
            .list_kubernetes_cluster_nodes(cluster_name)?
            .into_iter()
            .filter(|row| !row.pending_join && !row.pending_removal)
            .map(|row| row.node_id)
            .collect();
        let health = self.observe_health(cluster_name).await?;
        let mut view = RenewalView {
            members: health
                .nodes
                .iter()
                .filter(|n| serving.contains(&n.node_id))
                .map(|n| RenewalMember {
                    node_id: n.node_id.clone(),
                    control_plane: n.role != "worker",
                    cert_due: true,
                })
                .collect(),
            renewed: Vec::new(),
            restarted: Vec::new(),
            api_ready: health.api_reachable,
            etcd_healthy_cps: health
                .nodes
                .iter()
                .filter(|n| n.role != "worker" && n.etcd_healthy)
                .count(),
            applying: None,
            failed: None,
        };

        let cluster = self
            .db
            .get_kubernetes_cluster(cluster_name)?
            .context("cluster not found")?;
        let mut reissued = false;
        let mut summary = None;
        for _ in 0..(view.members.len() * 2 + 2) {
            match next_renewal_step(&view) {
                RenewalStep::Idle => {
                    summary = Some(format!(
                        "{NOTICE_CERTS} waiting: {}/{} nodes done",
                        view.restarted.len(),
                        view.members.len()
                    ));
                    break;
                }
                RenewalStep::Done => {
                    info!(
                        cluster = cluster_name,
                        nodes = view.restarted.len(),
                        "certificates renewed"
                    );
                    break;
                }
                RenewalStep::Failed { reason } => {
                    summary = Some(format!("{NOTICE_CERTS} failed: {reason}"));
                    break;
                }
                RenewalStep::RenewNode { node_id } => {
                    if !reissued {
                        self.reissue_leaf_certs(cluster_name).await?;
                        reissued = true;
                    }
                    self.db.set_kubernetes_cluster_status(
                        cluster_name,
                        "ready",
                        &format!("{NOTICE_CERTS}: rebuilding {node_id}"),
                    )?;
                    match self.rebuild_for_renewal(cluster_name, &node_id).await {
                        Ok(()) => {
                            view.renewed.push(node_id.clone());
                            for m in view.members.iter_mut().filter(|m| m.node_id == node_id) {
                                m.cert_due = false;
                            }
                            view.etcd_healthy_cps =
                                self.count_healthy_etcd_members(cluster_name).await?;
                        }
                        Err(e) => view.failed = Some(format!("{node_id}: {e:#}")),
                    }
                }
                RenewalStep::RestartUnits { node_id } => {
                    let control_plane = view
                        .members
                        .iter()
                        .any(|m| m.node_id == node_id && m.control_plane);
                    let units = certificate_units(
                        cluster_name,
                        &cluster.api_server_endpoint,
                        control_plane,
                    );
                    let address = self
                        .db
                        .get_node(&node_id)?
                        .map(|n| n.address)
                        .unwrap_or_default();
                    match self.nodes.restart_units(&address, &units).await {
                        Ok(()) => {
                            view.restarted.push(node_id.clone());
                            if control_plane {
                                if let Err(e) =
                                    self.wait_for_etcd_members(cluster_name, &node_id).await
                                {
                                    view.failed = Some(format!("{e:#}"));
                                }
                            }
                            view.etcd_healthy_cps =
                                self.count_healthy_etcd_members(cluster_name).await?;
                        }
                        Err(e) => view.failed = Some(format!("restart units on {node_id}: {e}")),
                    }
                }
            }
        }
        if summary.is_none() && reissued {
            let renewed = self.leaf_expiry(cluster_name)?;
            info!(
                cluster = cluster_name,
                ?renewed,
                "leaf certificates replaced on every node"
            );
            self.db
                .set_kubernetes_cluster_status(cluster_name, "ready", "certificates renewed")?;
        }
        Ok(summary)
    }

    /// Push the renewed configuration (public certs in the Nix, keys as
    /// secret files written ahead of the rebuild) and wait for it.
    async fn rebuild_for_renewal(&self, cluster_name: &str, node_id: &str) -> Result<()> {
        let apply_id = self.push_member_config(cluster_name, node_id).await?;
        let address = self
            .db
            .get_node(node_id)?
            .map(|n| n.address)
            .context("node not found")?;
        self.wait_for_apply(node_id, &address, &apply_id).await
    }

    // ---- backups ---------------------------------------------------------

    pub async fn take_snapshot(
        &self,
        cluster_name: &str,
        trigger: &str,
    ) -> Result<KubernetesEtcdSnapshotRow> {
        let root = self.backup_root.clone();
        self.take_snapshot_in(&root, cluster_name, trigger).await
    }

    pub(super) async fn take_snapshot_in(
        &self,
        root: &Path,
        cluster_name: &str,
        trigger: &str,
    ) -> Result<KubernetesEtcdSnapshotRow> {
        kcore_sanitize::validate_safe_segment(cluster_name)
            .map_err(|e| anyhow!("cluster name {cluster_name}: {e:?}"))?;
        let mut last_err = anyhow!("cluster has no healthy control plane");
        for row in self.db.list_kubernetes_cluster_nodes(cluster_name)? {
            if row.role == "worker" || row.pending_join || row.pending_removal {
                continue;
            }
            let ip = self.member_ip(&row)?;
            if !self
                .probe_etcd_member(cluster_name, &ip)
                .await
                .unwrap_or(false)
            {
                continue;
            }
            let address = self
                .db
                .get_node(&row.node_id)?
                .map(|n| n.address)
                .unwrap_or_default();
            if address.is_empty() {
                continue;
            }
            match self.nodes.etcd_snapshot(&address, cluster_name).await {
                Ok(bytes) if bytes.is_empty() => {
                    last_err = anyhow!("{} returned an empty snapshot", row.node_id);
                }
                Ok(bytes) => {
                    return self
                        .store_snapshot(root, cluster_name, trigger, &bytes)
                        .await
                }
                Err(e) => last_err = anyhow!("snapshot from {}: {e}", row.node_id),
            }
        }
        Err(last_err)
    }

    async fn store_snapshot(
        &self,
        root: &Path,
        cluster_name: &str,
        trigger: &str,
        bytes: &[u8],
    ) -> Result<KubernetesEtcdSnapshotRow> {
        use std::os::unix::fs::PermissionsExt;

        let now = OffsetDateTime::now_utc();
        let created_at = now.format(&Rfc3339)?;
        let stamp = created_at.replace([':', '-'], "");
        let short = uuid::Uuid::new_v4().simple().to_string();
        let id = format!("etcd-{}-{}", stamp.trim_end_matches('Z'), &short[..8]);
        let dir = root.join(cluster_name);
        tokio::fs::create_dir_all(&dir)
            .await
            .with_context(|| format!("create {}", dir.display()))?;
        tokio::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).await?;
        let path = dir.join(format!("{id}.db"));
        let tmp = dir.join(format!(".{id}.tmp"));
        tokio::fs::write(&tmp, bytes)
            .await
            .with_context(|| format!("write {}", tmp.display()))?;
        tokio::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600)).await?;
        tokio::fs::rename(&tmp, &path).await?;

        let row = KubernetesEtcdSnapshotRow {
            id,
            cluster_name: cluster_name.to_string(),
            created_at,
            sha256: kubernetes_backup::sha256_hex(bytes),
            size_bytes: bytes.len() as i64,
            path: path.to_string_lossy().into_owned(),
            trigger: trigger.to_string(),
        };
        if let Err(e) = self.db.insert_kubernetes_etcd_snapshot(&row) {
            let _ = tokio::fs::remove_file(&path).await;
            return Err(e.into());
        }
        info!(cluster = cluster_name, snapshot = %row.id, size = row.size_bytes, trigger, "etcd snapshot stored");
        Ok(row)
    }

    pub async fn delete_snapshot(&self, snapshot_id: &str) -> Result<bool> {
        let Some(row) = self.db.get_kubernetes_etcd_snapshot(snapshot_id)? else {
            return Ok(false);
        };
        match tokio::fs::remove_file(&row.path).await {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(anyhow!("remove {}: {e}", row.path)),
        }
        Ok(self.db.delete_kubernetes_etcd_snapshot(snapshot_id)?)
    }

    async fn reconcile_backup(&self, cluster_name: &str) -> Result<Option<String>> {
        let Some(ops) = self.db.get_kubernetes_cluster_ops(cluster_name)? else {
            return Ok(None);
        };
        let spec = match kubernetes_backup::parse_backup_spec(&ops.backup_json) {
            Ok(None) => return Ok(None),
            Ok(Some(spec)) => spec,
            Err(e) => return Ok(Some(format!("{NOTICE_BACKUP}: {e}"))),
        };
        let root = match spec.local_store_root(DEFAULT_BACKUP_ROOT) {
            Ok(root) => root,
            Err(e) => return Ok(Some(format!("{NOTICE_BACKUP}: {e}"))),
        };
        let snapshots = self.db.list_kubernetes_etcd_snapshots(cluster_name)?;
        let newest = snapshots.first().and_then(|s| parse_unix(&s.created_at));
        if kubernetes_backup::backup_due(newest, unix_now(), spec.interval_secs) {
            match self
                .take_snapshot_in(Path::new(&root), cluster_name, "scheduled")
                .await
            {
                Ok(row) => {
                    if let BackupTarget::S3 {
                        bucket,
                        prefix,
                        credentials_file,
                        endpoint,
                    } = &spec.target
                    {
                        if let Err(e) = upload_snapshot_s3(
                            &row.path,
                            bucket,
                            prefix,
                            &row.id,
                            credentials_file,
                            endpoint,
                        )
                        .await
                        {
                            return Ok(Some(format!("{NOTICE_BACKUP}: s3 upload: {e}")));
                        }
                    }
                }
                Err(e) => return Ok(Some(format!("{NOTICE_BACKUP}: {e:#}"))),
            }
        }
        self.prune_snapshots(cluster_name, &spec, &ops.pre_upgrade_snapshot_id)
            .await?;
        Ok(None)
    }

    pub(super) async fn prune_snapshots(
        &self,
        cluster_name: &str,
        spec: &BackupSpec,
        protected_id: &str,
    ) -> Result<()> {
        let ages: Vec<SnapshotAge> = self
            .db
            .list_kubernetes_etcd_snapshots(cluster_name)?
            .iter()
            .map(|s| SnapshotAge {
                id: s.id.clone(),
                taken_unix: parse_unix(&s.created_at).unwrap_or(0),
            })
            .collect();
        let protected: Vec<String> = if protected_id.is_empty() {
            Vec::new()
        } else {
            vec![protected_id.to_string()]
        };
        for id in kubernetes_backup::snapshots_to_prune(
            &ages,
            unix_now(),
            spec.retain_count,
            spec.retain_days,
            &protected,
        ) {
            if let BackupTarget::S3 {
                bucket,
                prefix,
                credentials_file,
                endpoint,
            } = &spec.target
            {
                if let Err(e) =
                    delete_snapshot_s3(bucket, prefix, &id, credentials_file, endpoint).await
                {
                    warn!(
                        cluster = cluster_name,
                        snapshot = %id,
                        error = %e,
                        "s3 snapshot prune failed"
                    );
                }
            }
            if let Err(e) = self.delete_snapshot(&id).await {
                warn!(cluster = cluster_name, snapshot = %id, error = %format!("{e:#}"), "snapshot prune failed");
            }
        }
        Ok(())
    }

    // ---- repair ----------------------------------------------------------

    async fn reconcile_repair(&self, cluster_name: &str) -> Result<Option<String>> {
        let Some(ops) = self.db.get_kubernetes_cluster_ops(cluster_name)? else {
            return Ok(None);
        };
        let policy = match kubernetes_repair::parse_repair_policy(&ops.repair_json) {
            Ok(None) => return Ok(None),
            Ok(Some(policy)) => policy,
            Err(e) => return Ok(Some(format!("{NOTICE_REPAIR}: {e}"))),
        };
        let rows = self.db.list_kubernetes_cluster_nodes(cluster_name)?;
        let busy = rows.iter().any(|r| r.pending_join || r.pending_removal);
        let health = self.observe_health(cluster_name).await?;
        if let Some(notice) = self
            .maybe_reinstall_control_plane(cluster_name, &health)
            .await?
        {
            return Ok(Some(notice));
        }
        let now = unix_now();
        let cps: Vec<CpHealth> = health
            .nodes
            .iter()
            .filter(|n| n.role != "worker")
            .map(|n| CpHealth {
                node_id: n.node_id.clone(),
                healthy: n.etcd_healthy,
                unhealthy_since: self.unhealthy_since(
                    cluster_name,
                    &n.node_id,
                    n.etcd_healthy,
                    now,
                ),
            })
            .collect();

        match kubernetes_repair::repair_decision(
            &cps,
            now,
            policy.after_secs,
            &policy.spare_pool,
            busy,
        ) {
            RepairDecision::None => Ok(None),
            RepairDecision::Alert { node_id, reason } => {
                Ok(Some(format!("{NOTICE_REPAIR}: {node_id}: {reason}")))
            }
            RepairDecision::Replace { old, new_spare } => {
                match self.begin_replacement(cluster_name, &old, &new_spare) {
                    Ok(()) => Ok(None),
                    Err(e) => Ok(Some(format!("{NOTICE_REPAIR}: {old}: {e:#}"))),
                }
            }
        }
    }

    /// Same node-id reinstall: the kcore node is Ready again but etcd on that
    /// host is gone/wrong while membership still lists the peer. Snapshot,
    /// wipe local etcd data, remove+re-add the member, then reconfigure.
    async fn maybe_reinstall_control_plane(
        &self,
        cluster_name: &str,
        health: &ClusterHealth,
    ) -> Result<Option<String>> {
        let rows = self.db.list_kubernetes_cluster_nodes(cluster_name)?;
        if rows.iter().any(|r| r.pending_join || r.pending_removal) {
            return Ok(None);
        }
        for member in health.nodes.iter().filter(|n| n.role != "worker") {
            if member.etcd_healthy {
                continue;
            }
            let Some(node) = self.db.get_node(&member.node_id)? else {
                continue;
            };
            if node.status != "ready" || node.approval_status != "approved" {
                continue;
            }
            let address = self.member_admin_address(cluster_name, &member.node_id)?;
            let empty = match self.nodes.etcd_data_empty(&address, cluster_name).await {
                Ok(empty) => empty,
                Err(e) => {
                    warn!(
                        cluster = cluster_name,
                        node = %member.node_id,
                        error = %e,
                        "etcd data empty probe failed"
                    );
                    continue;
                }
            };
            if !empty {
                continue;
            }
            // Host is Ready again with an empty etcd dir: same node-id reinstall.
            warn!(
                cluster = cluster_name,
                node = %member.node_id,
                "control plane host is ready with empty etcd; attempting reinstall rejoin"
            );
            if let Err(e) = self.take_snapshot(cluster_name, "pre-reinstall").await {
                warn!(
                    cluster = cluster_name,
                    error = %format!("{e:#}"),
                    "pre-reinstall snapshot skipped"
                );
            }
            if let Err(e) = self.nodes.wipe_etcd_data(&address, cluster_name).await {
                return Ok(Some(format!(
                    "{NOTICE_REPAIR}: wipe etcd on {}: {e}",
                    member.node_id
                )));
            }
            if let Err(e) = self.etcd_member_remove(cluster_name, &member.node_id).await {
                warn!(
                    cluster = cluster_name,
                    node = %member.node_id,
                    error = %format!("{e:#}"),
                    "etcd member remove during reinstall (continuing)"
                );
            }
            if let Err(e) = self.etcd_member_add(cluster_name, &member.node_id).await {
                return Ok(Some(format!(
                    "{NOTICE_REPAIR}: re-add etcd {}: {e:#}",
                    member.node_id
                )));
            }
            if let Err(e) = self.push_member_config(cluster_name, &member.node_id).await {
                return Ok(Some(format!(
                    "{NOTICE_REPAIR}: reconfigure {}: {e:#}",
                    member.node_id
                )));
            }
            self.unhealthy
                .lock()
                .expect("kubernetes unhealthy map")
                .remove(&format!("{cluster_name}/{}", member.node_id));
            return Ok(Some(format!(
                "{NOTICE_REPAIR}: reinstalled control plane {} rejoined etcd",
                member.node_id
            )));
        }
        Ok(None)
    }

    /// Time the control plane has been unhealthy, or None while it is healthy.
    fn unhealthy_since(
        &self,
        cluster_name: &str,
        node_id: &str,
        healthy: bool,
        now: i64,
    ) -> Option<i64> {
        let key = format!("{cluster_name}/{node_id}");
        let mut seen = self.unhealthy.lock().expect("kubernetes unhealthy map");
        if healthy {
            seen.remove(&key);
            None
        } else {
            Some(*seen.entry(key).or_insert(now))
        }
    }

    /// Start replacing `old` with `spare`: the scaling steps add the spare to
    /// etcd, wait for it, and remove the old member.
    pub(super) fn begin_replacement(
        &self,
        cluster_name: &str,
        old: &str,
        spare: &str,
    ) -> Result<()> {
        let spare_node = self
            .db
            .get_node(spare)?
            .with_context(|| format!("spare {spare} is not a registered kcore node"))?;
        if spare_node.approval_status != "approved" || spare_node.status != "ready" {
            anyhow::bail!("spare {spare} is not an approved, ready node");
        }
        let rows = self.db.list_kubernetes_cluster_nodes(cluster_name)?;
        let old_row = rows
            .iter()
            .find(|row| row.node_id == old)
            .with_context(|| format!("{old} is not a member"))?
            .clone();
        if self
            .db
            .list_kubernetes_nodes_by_node_id(spare)?
            .iter()
            .any(|row| row.cluster_name == cluster_name)
        {
            anyhow::bail!("spare {spare} is already a member");
        }
        warn!(
            cluster = cluster_name,
            old, spare, "replacing dead control plane"
        );
        self.db
            .upsert_kubernetes_cluster_node(&KubernetesClusterNodeRow {
                cluster_name: cluster_name.to_string(),
                node_id: spare.to_string(),
                role: "control-plane".to_string(),
                advertise_address: spare_node
                    .address
                    .split(':')
                    .next()
                    .unwrap_or("")
                    .to_string(),
                iface: old_row.iface.clone(),
                initial_cluster: false,
                join_existing: true,
                kubelet_ready: false,
                pending_join: true,
                pending_removal: false,
                k8s_node_name: String::new(),
            })?;
        let mut leaving = old_row;
        leaving.pending_removal = true;
        self.db.upsert_kubernetes_cluster_node(&leaving)?;
        self.db.set_kubernetes_cluster_status(
            cluster_name,
            "scaling",
            &format!("repair: replacing control plane {old} with {spare}"),
        )?;
        self.unhealthy
            .lock()
            .expect("kubernetes unhealthy map")
            .remove(&format!("{cluster_name}/{old}"));
        Ok(())
    }

    // ---- upgrade bookkeeping ---------------------------------------------

    /// When an upgrade starts (no member has moved yet), remember the version
    /// the cluster runs and take an etcd snapshot to roll back to. The
    /// snapshot is best effort: an upgrade is not blocked by a failed one.
    pub(super) async fn record_upgrade_start(&self, cluster_name: &str, view: &UpgradeView) {
        let desired = kubernetes_versions::effective(&view.desired);
        let Some(first) = view.members.first() else {
            return;
        };
        let current = kubernetes_versions::effective(&first.version);
        let untouched = view.members.iter().all(|m| {
            m.state == kubernetes_upgrade::STATE_IDLE
                && kubernetes_versions::effective(&m.version) == current
        });
        if !untouched || current == desired {
            return;
        }
        let result = async {
            let ops = self.db.get_kubernetes_cluster_ops(cluster_name)?;
            // Same base as the last recorded upgrade, or a rollback heading
            // back to the recorded version: keep what is stored.
            if ops
                .as_ref()
                .is_some_and(|o| o.previous_version == current || o.previous_version == desired)
            {
                return anyhow::Ok(());
            }
            self.db
                .set_kubernetes_cluster_previous_version(cluster_name, &current)?;
            let snapshot_id = match self.take_snapshot(cluster_name, "pre-upgrade").await {
                Ok(row) => row.id,
                Err(e) => {
                    warn!(cluster = cluster_name, error = %format!("{e:#}"), "pre-upgrade snapshot skipped");
                    String::new()
                }
            };
            self.db
                .set_kubernetes_cluster_pre_upgrade_snapshot_id(cluster_name, &snapshot_id)?;
            Ok(())
        }
        .await;
        if let Err(e) = result {
            warn!(cluster = cluster_name, error = %format!("{e:#}"), "could not record upgrade start");
        }
    }

    // ---- etcd restore ----------------------------------------------------

    /// Queue an etcd restore. The orchestrator loop drives [`reconcile_restore`].
    pub async fn begin_restore(&self, cluster_name: &str, snapshot_id: &str) -> Result<()> {
        let snapshot = self
            .db
            .get_kubernetes_etcd_snapshot(snapshot_id)?
            .filter(|row| row.cluster_name == cluster_name)
            .with_context(|| format!("snapshot {snapshot_id} not found for {cluster_name}"))?;
        if !Path::new(&snapshot.path).is_file() {
            anyhow::bail!(
                "snapshot file {} is missing on the controller",
                snapshot.path
            );
        }
        let mut members = self
            .db
            .list_kubernetes_cluster_nodes(cluster_name)?
            .into_iter()
            .filter(|row| row.role != "worker" && !row.pending_join)
            .collect::<Vec<_>>();
        members.sort_by(|a, b| a.node_id.cmp(&b.node_id));
        if members.is_empty() {
            anyhow::bail!("cluster has no control planes");
        }
        let mut initial_parts = Vec::new();
        for row in &members {
            let ip = self.member_ip(row)?;
            let host = self
                .db
                .get_node(&row.node_id)?
                .map(|n| {
                    if n.hostname.is_empty() {
                        row.node_id.clone()
                    } else {
                        n.hostname
                    }
                })
                .unwrap_or_else(|| row.node_id.clone());
            initial_parts.push(format!("{host}=https://{ip}:2380"));
        }
        let token = format!("kcore-restore-{}", uuid::Uuid::new_v4().simple());
        let view = kubernetes_backup::RestoreView {
            members: members.iter().map(|m| m.node_id.clone()).collect(),
            snapshot_available: true,
            api_stopped: vec![],
            etcd_stopped: vec![],
            restored: vec![],
            etcd_started: vec![],
            etcd_quorum: false,
            api_started: vec![],
            api_ready: false,
            failed: None,
            snapshot_id: snapshot.id,
            cluster_token: token,
            initial_cluster: initial_parts.join(","),
        };
        let json = serde_json::to_string(&view).context("serialize restore view")?;
        self.db
            .set_kubernetes_cluster_restore_json(cluster_name, &json)?;
        self.db.set_kubernetes_cluster_status(
            cluster_name,
            PHASE_RESTORING,
            &format!("restoring etcd from {}", view.snapshot_id),
        )?;
        Ok(())
    }

    pub async fn reconcile_restore(&self, cluster_name: &str) -> Result<()> {
        use kubernetes_backup::{next_restore_step, RestoreStep};

        let Some(ops) = self.db.get_kubernetes_cluster_ops(cluster_name)? else {
            return Ok(());
        };
        if ops.restore_json.trim().is_empty() {
            return Ok(());
        }
        let mut view: kubernetes_backup::RestoreView =
            serde_json::from_str(&ops.restore_json).context("parse restore_json")?;
        let cluster = self
            .db
            .get_kubernetes_cluster(cluster_name)?
            .context("cluster not found")?;
        let snapshot = self
            .db
            .get_kubernetes_etcd_snapshot(&view.snapshot_id)?
            .context("restore snapshot missing")?;
        let bytes = tokio::fs::read(&snapshot.path)
            .await
            .with_context(|| format!("read {}", snapshot.path))?;
        if kubernetes_backup::sha256_hex(&bytes) != snapshot.sha256 {
            view.failed = Some("snapshot checksum mismatch on controller disk".into());
        }

        let step = next_restore_step(&view);
        match step {
            RestoreStep::StopApi { node_id } => {
                self.manage_member_units(cluster_name, &node_id, &["kube-apiserver"], "stop")
                    .await?;
                view.api_stopped.push(node_id);
            }
            RestoreStep::StopEtcd { node_id } => {
                self.manage_member_units(cluster_name, &node_id, &["etcd"], "stop")
                    .await?;
                view.etcd_stopped.push(node_id);
            }
            RestoreStep::RestoreMember { node_id } => {
                let address = self.member_admin_address(cluster_name, &node_id)?;
                self.nodes
                    .etcd_restore(
                        &address,
                        cluster_name,
                        &bytes,
                        &view.initial_cluster,
                        &view.cluster_token,
                    )
                    .await
                    .map_err(|e| anyhow!("etcd restore on {node_id}: {e}"))?;
                view.restored.push(node_id);
            }
            RestoreStep::StartEtcd { node_id } => {
                self.manage_member_units(cluster_name, &node_id, &["etcd"], "start")
                    .await?;
                view.etcd_started.push(node_id);
            }
            RestoreStep::WaitQuorum => {
                let healthy = self.count_healthy_etcd_members(cluster_name).await?;
                let need = view.members.len() / 2 + 1;
                if healthy >= need {
                    view.etcd_quorum = true;
                } else {
                    self.db.set_kubernetes_cluster_status(
                        cluster_name,
                        PHASE_RESTORING,
                        &format!("waiting for etcd quorum ({healthy}/{need})"),
                    )?;
                    self.persist_restore(cluster_name, &view)?;
                    return Ok(());
                }
            }
            RestoreStep::StartApi { node_id } => {
                self.manage_member_units(cluster_name, &node_id, &["kube-apiserver"], "start")
                    .await?;
                view.api_started.push(node_id);
            }
            RestoreStep::WaitApi => {
                let pki = self
                    .db
                    .load_kubernetes_pki(cluster_name)?
                    .context("PKI missing during restore")?;
                let endpoint = cluster
                    .api_server_endpoint
                    .trim()
                    .trim_start_matches("https://")
                    .trim_start_matches("http://");
                let host = endpoint.split(':').next().unwrap_or("");
                let ready = self
                    .nodes
                    .apiserver_ready(endpoint, host, &pki.ca_cert)
                    .await
                    .unwrap_or(false);
                if ready {
                    view.api_ready = true;
                } else {
                    self.db.set_kubernetes_cluster_status(
                        cluster_name,
                        PHASE_RESTORING,
                        "waiting for API server after restore",
                    )?;
                    self.persist_restore(cluster_name, &view)?;
                    return Ok(());
                }
            }
            RestoreStep::Done => {
                self.db
                    .set_kubernetes_cluster_restore_json(cluster_name, "")?;
                self.db.set_kubernetes_cluster_status(
                    cluster_name,
                    "ready",
                    &format!("restored from {}", view.snapshot_id),
                )?;
                info!(cluster = cluster_name, snapshot = %view.snapshot_id, "etcd restore finished");
                return Ok(());
            }
            RestoreStep::Failed { reason } => {
                self.db.set_kubernetes_cluster_status(
                    cluster_name,
                    "degraded",
                    &format!("etcd restore failed: {reason}"),
                )?;
                self.persist_restore(cluster_name, &view)?;
                anyhow::bail!("etcd restore failed: {reason}");
            }
        }
        self.persist_restore(cluster_name, &view)?;
        self.db.set_kubernetes_cluster_status(
            cluster_name,
            PHASE_RESTORING,
            &format!("restoring etcd from {}", view.snapshot_id),
        )?;
        Ok(())
    }

    fn persist_restore(
        &self,
        cluster_name: &str,
        view: &kubernetes_backup::RestoreView,
    ) -> Result<()> {
        let json = serde_json::to_string(view)?;
        self.db
            .set_kubernetes_cluster_restore_json(cluster_name, &json)?;
        Ok(())
    }

    fn member_admin_address(&self, cluster_name: &str, node_id: &str) -> Result<String> {
        let row = self
            .db
            .list_kubernetes_cluster_nodes(cluster_name)?
            .into_iter()
            .find(|r| r.node_id == node_id)
            .with_context(|| format!("{node_id} is not a member"))?;
        let node = self
            .db
            .get_node(node_id)?
            .with_context(|| format!("node {node_id} missing"))?;
        if !node.address.is_empty() {
            return Ok(node.address);
        }
        let ip = self.member_ip(&row)?;
        Ok(format!("{ip}:9091"))
    }

    async fn manage_member_units(
        &self,
        cluster_name: &str,
        node_id: &str,
        base_units: &[&str],
        action: &str,
    ) -> Result<()> {
        let cluster = self
            .db
            .get_kubernetes_cluster(cluster_name)?
            .context("cluster not found")?;
        let units = certificate_units(cluster_name, &cluster.api_server_endpoint, true)
            .into_iter()
            .filter(|u| {
                base_units
                    .iter()
                    .any(|base| u == base || u.starts_with(&format!("{base}-")))
            })
            .collect::<Vec<_>>();
        let address = self.member_admin_address(cluster_name, node_id)?;
        self.nodes
            .manage_units(&address, &units, action)
            .await
            .map_err(|e| anyhow!("{action} {units:?} on {node_id}: {e}"))?;
        Ok(())
    }

    /// Ceph CSI YAML for the cluster, or empty when storage is off.
    pub fn ceph_csi_manifest_for(&self, cluster_name: &str) -> String {
        let Ok(Some(ops)) = self.db.get_kubernetes_cluster_ops(cluster_name) else {
            return String::new();
        };
        if ops.storage_json.trim().is_empty() {
            return String::new();
        }
        match serde_json::from_str::<crate::kubernetes_csi::CephStorage>(&ops.storage_json) {
            Ok(storage) => crate::kubernetes_csi::manifests(&storage, cluster_name),
            Err(e) => {
                warn!(cluster = cluster_name, error = %e, "invalid storage_json");
                String::new()
            }
        }
    }

    /// Rotate encryption-at-rest keys, push the new secretbox config, restart
    /// API servers, then rewrite every Secret so the new key is primary.
    pub async fn rotate_encryption(&self, cluster_name: &str) -> Result<String> {
        let ops = self
            .db
            .get_kubernetes_cluster_ops(cluster_name)?
            .context("cluster ops missing")?;
        let next =
            match crate::kubernetes_hardening::EncryptionKeys::from_json(&ops.encryption_json) {
                Some(keys) => keys.rotate(),
                None => crate::kubernetes_hardening::EncryptionKeys::new_random(),
            };
        self.db
            .set_kubernetes_cluster_encryption_json(cluster_name, &next.to_json())?;
        let members = self.db.list_kubernetes_cluster_nodes(cluster_name)?;
        let cps: Vec<_> = members
            .iter()
            .filter(|m| m.role == "control-plane" && !m.pending_removal && !m.pending_join)
            .cloned()
            .collect();
        if cps.is_empty() {
            anyhow::bail!("no control plane to roll encryption onto");
        }
        let yaml = next.provider_config_yaml();
        let path = format!(
            "{}/encryption-config.yaml",
            crate::nixgen::kubernetes_pki_key_dir(cluster_name)
        );
        for cp in &cps {
            let address = self.member_admin_address(cluster_name, &cp.node_id)?;
            self.nodes
                .write_secrets(
                    &address,
                    &[SecretFile {
                        path: path.clone(),
                        mode: 0o600,
                        content: yaml.as_bytes().to_vec(),
                    }],
                )
                .await
                .map_err(|e| anyhow!("write encryption config on {}: {e}", cp.node_id))?;
            let units = certificate_units(cluster_name, &self.api_endpoint(cluster_name)?, true)
                .into_iter()
                .filter(|u| u.starts_with("kube-apiserver"))
                .collect::<Vec<_>>();
            self.nodes
                .restart_units(&address, &units)
                .await
                .map_err(|e| anyhow!("restart apiserver on {}: {e}", cp.node_id))?;
        }
        let rewrite_address = self.member_admin_address(cluster_name, &cps[0].node_id)?;
        let rewritten = self.rewrite_all_secrets(&rewrite_address).await?;
        Ok(format!(
            "encryption key {} rolled onto {}; rewrote {rewritten} secrets on {}",
            next.current_id,
            cps.iter()
                .map(|c| c.node_id.as_str())
                .collect::<Vec<_>>()
                .join(","),
            cps[0].node_id
        ))
    }

    /// Page through every Secret and PUT it back so the apiserver re-encrypts
    /// with the current primary key (same effect as `kubectl get secrets -A
    /// -o json | kubectl replace -f -`).
    async fn rewrite_all_secrets(&self, address: &str) -> Result<usize> {
        let mut rewritten = 0usize;
        let mut cont = String::new();
        loop {
            let path = if cont.is_empty() {
                "/api/v1/secrets?limit=500".to_string()
            } else {
                format!(
                    "/api/v1/secrets?limit=500&continue={}",
                    urlencoding_path(&cont)
                )
            };
            let (status, body) = self
                .nodes
                .kube(address, "GET", &path, "", "")
                .await
                .map_err(|e| anyhow!("list secrets: {e}"))?;
            if status != 200 {
                anyhow::bail!("list secrets returned {status}: {body}");
            }
            let v: serde_json::Value = serde_json::from_str(&body).context("parse secrets list")?;
            let items = v
                .get("items")
                .and_then(|i| i.as_array())
                .cloned()
                .unwrap_or_default();
            for item in items {
                let ns = item
                    .pointer("/metadata/namespace")
                    .and_then(|x| x.as_str())
                    .unwrap_or("default");
                let name = item
                    .pointer("/metadata/name")
                    .and_then(|x| x.as_str())
                    .unwrap_or("");
                if name.is_empty() {
                    continue;
                }
                let put_path = format!("/api/v1/namespaces/{ns}/secrets/{name}");
                let payload = item.to_string();
                match self
                    .nodes
                    .kube(address, "PUT", &put_path, &payload, "application/json")
                    .await
                {
                    Ok((200..=299, _)) => rewritten += 1,
                    Ok((code, resp)) => warn!(
                        secret = %format!("{ns}/{name}"),
                        status = code,
                        body = %resp,
                        "secret rewrite failed"
                    ),
                    Err(e) => {
                        warn!(secret = %format!("{ns}/{name}"), error = %e, "secret rewrite failed")
                    }
                }
            }
            let next = v
                .pointer("/metadata/continue")
                .and_then(|c| c.as_str())
                .unwrap_or("")
                .to_string();
            if next.is_empty() {
                break;
            }
            cont = next;
        }
        Ok(rewritten)
    }

    fn api_endpoint(&self, cluster_name: &str) -> Result<String> {
        Ok(self
            .db
            .get_kubernetes_cluster(cluster_name)?
            .context("cluster missing")?
            .api_server_endpoint)
    }

    /// Reverse-order rollback driven by [`kubernetes_rollback::next_rollback_step`].
    pub async fn reconcile_rollback(&self, cluster_name: &str) -> Result<()> {
        use crate::kubernetes_rollback::{
            next_rollback_step, RollbackMember, RollbackStep, RollbackView,
        };

        let ops = self
            .db
            .get_kubernetes_cluster_ops(cluster_name)?
            .context("cluster ops missing")?;
        if ops.previous_version.trim().is_empty() {
            anyhow::bail!("no previous version recorded");
        }
        let mut cfg = self
            .db
            .get_kubernetes_cluster_config(cluster_name)?
            .context("cluster config missing")?;
        cfg.kubernetes_version = ops.previous_version.clone();
        self.db.upsert_kubernetes_cluster_config(&cfg)?;

        let upgrade = self.observe_upgrade(cluster_name).await?;
        let upgrade_order: Vec<String> =
            upgrade.members.iter().map(|m| m.node_id.clone()).collect();
        let members: Vec<RollbackMember> = upgrade
            .members
            .iter()
            .map(|m| RollbackMember {
                node_id: m.node_id.clone(),
                control_plane: m.role != "worker",
                version: m.version.clone(),
                etcd_healthy: m.etcd_healthy,
                apply_running: m.state == crate::kubernetes_upgrade::STATE_APPLYING,
            })
            .collect();
        let view = RollbackView {
            previous_version: ops.previous_version.clone(),
            upgrade_order,
            members,
            api_ready: upgrade.api_ready,
            failed: None,
        };
        match next_rollback_step(&view) {
            RollbackStep::Wait { message, .. } => {
                self.db.set_kubernetes_cluster_status(
                    cluster_name,
                    PHASE_ROLLING_BACK,
                    &message,
                )?;
            }
            RollbackStep::Rollback {
                node_id,
                to_version,
            } => {
                let mut row = self.upgrade_row(cluster_name, &node_id)?;
                row.kubernetes_version = to_version.clone();
                row.upgrade_state = crate::kubernetes_upgrade::STATE_APPLYING.to_string();
                self.db.set_kubernetes_node_upgrade(cluster_name, &row)?;
                match self.push_member_config(cluster_name, &node_id).await {
                    Ok(apply_id) => {
                        row.upgrade_apply_id = if apply_id.is_empty() {
                            "untracked".into()
                        } else {
                            apply_id
                        };
                        self.db.set_kubernetes_node_upgrade(cluster_name, &row)?;
                        self.db.set_kubernetes_cluster_status(
                            cluster_name,
                            PHASE_ROLLING_BACK,
                            &format!("rolling {node_id} back to Kubernetes {to_version}"),
                        )?;
                    }
                    Err(e) => {
                        let snap = if ops.pre_upgrade_snapshot_id.is_empty() {
                            String::new()
                        } else {
                            format!(
                                " If the API cannot serve, restore snapshot {}.",
                                ops.pre_upgrade_snapshot_id
                            )
                        };
                        self.db.set_kubernetes_cluster_status(
                            cluster_name,
                            "degraded",
                            &format!("rollback of {node_id} failed: {e:#}.{snap}"),
                        )?;
                        return Err(e);
                    }
                }
            }
            RollbackStep::Done => {
                self.db.set_kubernetes_cluster_status(
                    cluster_name,
                    "ready",
                    &format!(
                        "rolled back to Kubernetes {}",
                        crate::kubernetes_versions::effective(&ops.previous_version)
                    ),
                )?;
            }
            RollbackStep::Failed { reason } => {
                self.db.set_kubernetes_cluster_status(
                    cluster_name,
                    "degraded",
                    &format!("rollback failed: {reason}"),
                )?;
            }
        }
        Ok(())
    }
}

/// Percent-encode a continue token for the Kubernetes list API.
fn urlencoding_path(s: &str) -> String {
    let mut out = String::with_capacity(s.len() * 3);
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char);
            }
            _ => {
                const HEX: &[u8; 16] = b"0123456789ABCDEF";
                out.push('%');
                out.push(HEX[(b >> 4) as usize] as char);
                out.push(HEX[(b & 0x0f) as usize] as char);
            }
        }
    }
    out
}

/// Upload a snapshot file to S3 (SigV4 PutObject) using credentials JSON:
/// `{"access_key","secret_key","endpoint"?,"region"?}`.
async fn upload_snapshot_s3(
    local_path: &str,
    bucket: &str,
    prefix: &str,
    id: &str,
    credentials_file: &str,
    endpoint: &str,
) -> Result<(), String> {
    let raw = tokio::fs::read_to_string(credentials_file)
        .await
        .map_err(|e| format!("read {credentials_file}: {e}"))?;
    let creds = crate::kubernetes_s3::S3Credentials::from_json(&raw, endpoint)
        .map_err(|e| e.to_string())?;
    let key = crate::kubernetes_s3::object_key(prefix, id);
    let body = tokio::fs::read(local_path)
        .await
        .map_err(|e| format!("read {local_path}: {e}"))?;
    crate::kubernetes_s3::put_object(&creds, bucket, &key, &body)
        .await
        .map_err(|e| e.to_string())
}

/// Delete a snapshot object from S3 before pruning local metadata.
async fn delete_snapshot_s3(
    bucket: &str,
    prefix: &str,
    id: &str,
    credentials_file: &str,
    endpoint: &str,
) -> Result<(), String> {
    let raw = tokio::fs::read_to_string(credentials_file)
        .await
        .map_err(|e| format!("read {credentials_file}: {e}"))?;
    let creds = crate::kubernetes_s3::S3Credentials::from_json(&raw, endpoint)
        .map_err(|e| e.to_string())?;
    let key = crate::kubernetes_s3::object_key(prefix, id);
    crate::kubernetes_s3::delete_object(&creds, bucket, &key)
        .await
        .map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::{KubernetesClusterConfigRow, KubernetesClusterRow, NodeRow};
    use crate::kubernetes_nodes::Fake;

    fn node(db: &Database, id: &str, ip: &str) {
        db.upsert_node(&NodeRow {
            id: id.to_string(),
            hostname: id.to_string(),
            address: format!("{ip}:9091"),
            cpu_cores: 4,
            memory_bytes: 8_000_000_000,
            status: "ready".to_string(),
            last_heartbeat: String::new(),
            gateway_interface: "br0".to_string(),
            cpu_used: 0,
            memory_used: 0,
            storage_backend: "filesystem".to_string(),
            disable_vxlan: false,
            approval_status: "approved".to_string(),
            cert_expiry_days: 300,
            luks_method: String::new(),
            dc_id: "DC1".to_string(),
        })
        .unwrap();
    }

    fn member(cluster: &str, id: &str, ip: &str) -> KubernetesClusterNodeRow {
        KubernetesClusterNodeRow {
            cluster_name: cluster.to_string(),
            node_id: id.to_string(),
            role: "control-plane".to_string(),
            advertise_address: ip.to_string(),
            iface: "br0".to_string(),
            initial_cluster: id == "cp1",
            join_existing: false,
            kubelet_ready: true,
            pending_join: false,
            pending_removal: false,
            k8s_node_name: String::new(),
        }
    }

    /// A ready three-member cluster with real PKI, so renders work.
    async fn ready_cluster() -> (Database, Arc<Fake>, KubernetesOrchestrator) {
        let db = Database::open(":memory:").unwrap();
        let ips = [
            ("cp1", "10.0.0.1"),
            ("cp2", "10.0.0.2"),
            ("cp3", "10.0.0.3"),
        ];
        for (id, ip) in ips {
            node(&db, id, ip);
        }
        db.upsert_kubernetes_cluster(&KubernetesClusterRow {
            name: "lab".to_string(),
            generation: 1,
            cluster_cidr: "10.244.0.0/16".to_string(),
            service_cidr: "10.96.0.0/12".to_string(),
            cluster_dns: "10.96.0.10".to_string(),
            api_server_endpoint: "10.0.0.100:6443".to_string(),
            cni: "cilium".to_string(),
            phase: "ready".to_string(),
            status_message: String::new(),
            ready_control_planes: 3,
            ready_workers: 0,
            created_at: String::new(),
            updated_at: String::new(),
        })
        .unwrap();
        db.upsert_kubernetes_cluster_config(&KubernetesClusterConfigRow {
            cluster_name: "lab".to_string(),
            ha_enabled: false,
            vip_address: String::new(),
            vip_interface: String::new(),
            cilium_mode: "native-routing".to_string(),
            cilium_mtu: 1500,
            cilium_tunnel: "disabled".to_string(),
            cilium_auto_direct_routes: true,
            cilium_lb_ip_pools: String::new(),
            local_provisioner_json: String::new(),
            schedule_on_control_plane: false,
            kubernetes_version: String::new(),
        })
        .unwrap();
        for (id, ip) in ips {
            db.upsert_kubernetes_cluster_node(&member("lab", id, ip))
                .unwrap();
        }
        let infos: Vec<NodeInfo> = ips
            .iter()
            .map(|(id, ip)| NodeInfo {
                node_id: id.to_string(),
                hostname: id.to_string(),
                ip_address: ip.to_string(),
            })
            .collect();
        let pki = kubernetes_pki::generate_kubernetes_pki(
            "lab",
            "10.0.0.100:6443",
            "10.96.0.0/12",
            &infos,
        )
        .unwrap();
        db.store_kubernetes_pki("lab", &pki).unwrap();
        let fake = Fake::new(&["10.0.0.1", "10.0.0.2", "10.0.0.3"]);
        let orch =
            KubernetesOrchestrator::with_nodes(db.clone(), NodeClients::new(None), fake.clone());
        (db, fake, orch)
    }

    #[test]
    fn units_follow_the_cluster_suffix() {
        assert_eq!(
            certificate_units("lab", "10.0.0.1:6443", false),
            vec!["kubelet"]
        );
        let extra = certificate_units("b", "10.0.0.1:7443", true);
        assert!(extra.contains(&"etcd-b".to_string()));
        assert!(extra.contains(&"kube-apiserver-b".to_string()));
    }

    #[tokio::test]
    async fn rendering_is_stable_so_drift_is_not_invented() {
        let (_db, _fake, orch) = ready_cluster().await;
        let pki = orch.db.load_kubernetes_pki("lab").unwrap().unwrap();
        let a = orch.drift_hash("lab", "cp1", &pki).await.unwrap();
        let b = orch.drift_hash("lab", "cp1", &pki).await.unwrap();
        assert_eq!(a, b);
        assert_ne!(a, orch.drift_hash("lab", "cp2", &pki).await.unwrap());
    }

    #[tokio::test]
    async fn drift_baseline_then_change_moves_the_cluster_to_reconfiguring() {
        let (db, _fake, orch) = ready_cluster().await;
        assert!(matches!(
            orch.reconcile_drift("lab").await.unwrap(),
            DriftOutcome::Clean
        ));
        let hashes = db.list_kubernetes_node_rendered_hashes("lab").unwrap();
        assert!(hashes.values().all(|h| h.len() == 64));
        assert!(matches!(
            orch.reconcile_drift("lab").await.unwrap(),
            DriftOutcome::Clean
        ));

        db.set_kubernetes_node_rendered_hash("lab", "cp2", &"0".repeat(64))
            .unwrap();
        assert!(matches!(
            orch.reconcile_drift("lab").await.unwrap(),
            DriftOutcome::Reconciling
        ));
        let cluster = db.get_kubernetes_cluster("lab").unwrap().unwrap();
        assert_eq!(cluster.phase, PHASE_RECONFIGURING);
        assert!(cluster.status_message.contains("cp2"));
    }

    #[tokio::test]
    async fn drift_with_auto_reconcile_off_only_reports() {
        let (db, _fake, orch) = ready_cluster().await;
        orch.reconcile_drift("lab").await.unwrap();
        db.set_kubernetes_cluster_auto_reconcile("lab", false)
            .unwrap();
        db.set_kubernetes_node_rendered_hash("lab", "cp1", &"0".repeat(64))
            .unwrap();
        match orch.reconcile_drift("lab").await.unwrap() {
            DriftOutcome::Pending(message) => assert!(message.starts_with(NOTICE_DRIFT)),
            _ => panic!("expected pending drift"),
        }
        assert_eq!(
            db.get_kubernetes_cluster("lab").unwrap().unwrap().phase,
            "ready"
        );
    }

    #[tokio::test]
    async fn a_successful_apply_records_the_rendered_hash() {
        let (db, _fake, orch) = ready_cluster().await;
        let apply_id = orch.push_member_config("lab", "cp3").await.unwrap();
        assert!(!apply_id.is_empty());
        let stored = db.list_kubernetes_node_rendered_hashes("lab").unwrap();
        assert_eq!(stored["cp3"].len(), 64);
        assert_eq!(stored["cp1"], "");
    }

    #[tokio::test]
    async fn scheduled_backup_stores_prunes_and_skips_until_due() {
        let (db, fake, orch) = ready_cluster().await;
        let spec =
            r#"{"interval_secs":3600,"retain_count":2,"retain_days":0,"target":{"kind":"local"}}"#;
        db.set_kubernetes_cluster_backup_json("lab", spec).unwrap();
        let dir = tempfile::tempdir().unwrap();

        let first = orch
            .take_snapshot_in(dir.path(), "lab", "scheduled")
            .await
            .unwrap();
        assert!(std::path::Path::new(&first.path).exists());
        assert_eq!(
            first.sha256,
            kubernetes_backup::sha256_hex(&std::fs::read(&first.path).unwrap())
        );
        assert_eq!(fake.calls_named("etcd_snapshot").len(), 1);

        assert!(orch.reconcile_backup("lab").await.unwrap().is_none());
        assert_eq!(
            fake.calls_named("etcd_snapshot").len(),
            1,
            "a fresh snapshot makes the next one not due"
        );

        for _ in 0..3 {
            orch.take_snapshot_in(dir.path(), "lab", "manual")
                .await
                .unwrap();
        }
        let parsed = kubernetes_backup::parse_backup_spec(spec).unwrap().unwrap();
        orch.prune_snapshots("lab", &parsed, &first.id)
            .await
            .unwrap();
        let left = db.list_kubernetes_etcd_snapshots("lab").unwrap();
        assert!(
            left.iter().any(|s| s.id == first.id),
            "protected id survives"
        );
        assert!(left.len() <= 3);
        for row in &left {
            assert!(std::path::Path::new(&row.path).exists());
        }
    }

    #[tokio::test]
    async fn backup_without_a_healthy_control_plane_reports_a_notice() {
        let (db, fake, orch) = ready_cluster().await;
        db.set_kubernetes_cluster_backup_json(
            "lab",
            r#"{"interval_secs":60,"retain_count":1,"retain_days":0,"target":{"kind":"local"}}"#,
        )
        .unwrap();
        fake.etcd_members.lock().unwrap().clear();
        let notice = orch.reconcile_backup("lab").await.unwrap().unwrap();
        assert!(notice.starts_with(NOTICE_BACKUP), "{notice}");
    }

    #[tokio::test]
    async fn dead_control_plane_is_replaced_by_a_registered_spare() {
        let (db, fake, orch) = ready_cluster().await;
        node(&db, "spare", "10.0.0.9");
        db.set_kubernetes_cluster_repair_json("lab", r#"{"after_secs":60,"spare_pool":["spare"]}"#)
            .unwrap();
        fake.etcd_members
            .lock()
            .unwrap()
            .insert("10.0.0.3".to_string(), false);
        // First sight starts the clock; nothing is replaced yet.
        assert!(orch.reconcile_repair("lab").await.unwrap().is_none());
        assert_eq!(
            db.get_kubernetes_cluster("lab").unwrap().unwrap().phase,
            "ready"
        );

        orch.unhealthy
            .lock()
            .unwrap()
            .insert("lab/cp3".to_string(), unix_now() - 120);
        assert!(orch.reconcile_repair("lab").await.unwrap().is_none());
        let cluster = db.get_kubernetes_cluster("lab").unwrap().unwrap();
        assert_eq!(cluster.phase, "scaling");
        let rows = db.list_kubernetes_cluster_nodes("lab").unwrap();
        let old = rows.iter().find(|r| r.node_id == "cp3").unwrap();
        let new = rows.iter().find(|r| r.node_id == "spare").unwrap();
        assert!(old.pending_removal);
        assert!(new.pending_join && new.join_existing);
        assert_eq!(new.advertise_address, "10.0.0.9");
    }

    #[tokio::test]
    async fn repair_alerts_when_no_spare_is_registered() {
        let (db, fake, orch) = ready_cluster().await;
        db.set_kubernetes_cluster_repair_json("lab", r#"{"after_secs":60,"spare_pool":["ghost"]}"#)
            .unwrap();
        fake.etcd_members
            .lock()
            .unwrap()
            .insert("10.0.0.2".to_string(), false);
        orch.unhealthy
            .lock()
            .unwrap()
            .insert("lab/cp2".to_string(), unix_now() - 120);
        let notice = orch.reconcile_repair("lab").await.unwrap().unwrap();
        assert!(notice.starts_with(NOTICE_REPAIR), "{notice}");
        assert!(notice.contains("ghost"));
        assert_eq!(
            db.get_kubernetes_cluster("lab").unwrap().unwrap().phase,
            "ready"
        );
    }

    #[tokio::test]
    async fn reinstall_rejoins_when_etcd_data_is_empty() {
        let (db, fake, orch) = ready_cluster().await;
        db.set_kubernetes_cluster_repair_json("lab", r#"{"after_secs":60,"spare_pool":[]}"#)
            .unwrap();
        fake.etcd_members
            .lock()
            .unwrap()
            .insert("10.0.0.3".to_string(), false);
        let addr = db.get_node("cp3").unwrap().unwrap().address;
        fake.etcd_empty.lock().unwrap().insert(addr);
        let notice = orch.reconcile_repair("lab").await.unwrap().unwrap();
        assert!(notice.contains("reinstalled"), "{notice}");
        assert!(!fake.calls_named("wipe_etcd_data").is_empty());
    }

    #[tokio::test]
    async fn notices_replace_only_their_own_messages() {
        let (db, _fake, orch) = ready_cluster().await;
        db.set_kubernetes_cluster_status("lab", "ready", "etcd quorum and API server ready")
            .unwrap();
        orch.publish_notices("lab", &[]).unwrap();
        assert_eq!(
            db.get_kubernetes_cluster("lab")
                .unwrap()
                .unwrap()
                .status_message,
            "etcd quorum and API server ready"
        );
        orch.publish_notices("lab", &[format!("{NOTICE_BACKUP}: boom")])
            .unwrap();
        assert!(db
            .get_kubernetes_cluster("lab")
            .unwrap()
            .unwrap()
            .status_message
            .starts_with(NOTICE_BACKUP));
        orch.publish_notices("lab", &[]).unwrap();
        assert_eq!(
            db.get_kubernetes_cluster("lab")
                .unwrap()
                .unwrap()
                .status_message,
            "cluster ready"
        );
    }

    #[tokio::test]
    async fn renewal_waits_until_due_and_then_rolls_every_node() {
        let (db, fake, orch) = ready_cluster().await;
        assert!(orch
            .reconcile_cert_renewal("lab", false)
            .await
            .unwrap()
            .is_none());
        assert!(fake.calls_named("apply").is_empty());

        let before = db.load_kubernetes_pki("lab").unwrap().unwrap();
        db.set_kubernetes_pki_leaf_not_after("lab", "2000-01-01T00:00:00Z")
            .unwrap();
        let notice = orch.reconcile_cert_renewal("lab", false).await.unwrap();
        assert!(notice.is_none(), "{notice:?}");

        let after = db.load_kubernetes_pki("lab").unwrap().unwrap();
        assert_eq!(after.ca_cert, before.ca_cert);
        assert_ne!(after.apiserver_cert, before.apiserver_cert);
        assert_eq!(fake.calls_named("apply").len(), 3);
        let restarted = fake.units_restarted.lock().unwrap().clone();
        assert_eq!(restarted.len(), 3);
        assert!(restarted
            .iter()
            .all(|(_, units)| units.contains(&"etcd".to_string())));
        let days =
            (orch.leaf_expiry("lab").unwrap().unwrap() - OffsetDateTime::now_utc()).whole_days();
        assert!(days > 300, "leaf expiry was not renewed: {days}");
        let hashes = db.list_kubernetes_node_rendered_hashes("lab").unwrap();
        assert!(hashes.values().all(|h| h.len() == 64));
    }

    #[tokio::test]
    async fn renewal_stops_when_a_rebuild_fails() {
        let (db, fake, orch) = ready_cluster().await;
        db.set_kubernetes_pki_leaf_not_after("lab", "2000-01-01T00:00:00Z")
            .unwrap();
        fake.apply_fail
            .lock()
            .unwrap()
            .insert("10.0.0.1:9091".to_string(), "boom".to_string());
        // The agent address comes from the node row, which carries :9091.
        let notice = orch
            .reconcile_cert_renewal("lab", false)
            .await
            .unwrap()
            .unwrap();
        assert!(notice.starts_with(NOTICE_CERTS), "{notice}");
        assert!(fake.units_restarted.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn upgrade_start_records_previous_version_and_a_snapshot_once() {
        let (db, fake, orch) = ready_cluster().await;
        let dir = tempfile::tempdir().unwrap();
        let orch = orch.with_backup_root(dir.path());
        let view = UpgradeView {
            desired: "1.36.6".to_string(),
            api_ready: true,
            members: vec![kubernetes_upgrade::UpgradeMember {
                node_id: "cp1".to_string(),
                role: "control-plane".to_string(),
                version: "1.35.2".to_string(),
                state: kubernetes_upgrade::STATE_IDLE.to_string(),
                cordoned: false,
                apply: ApplyProgress::Unknown,
                apply_message: String::new(),
                kubelet_version: String::new(),
                node_ready: true,
                etcd_healthy: true,
            }],
        };
        orch.record_upgrade_start("lab", &view).await;
        let ops = db.get_kubernetes_cluster_ops("lab").unwrap().unwrap();
        assert_eq!(ops.previous_version, "1.35.2");
        let snapshots = db.list_kubernetes_etcd_snapshots("lab").unwrap();
        assert_eq!(snapshots.len(), 1);
        assert_eq!(snapshots[0].trigger, "pre-upgrade");
        assert_eq!(ops.pre_upgrade_snapshot_id, snapshots[0].id);
        orch.record_upgrade_start("lab", &view).await;
        assert_eq!(fake.calls_named("etcd_snapshot").len(), 1, "recorded once");
    }
}
