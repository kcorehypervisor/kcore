//! etcd backup policy and the restore sequence.
//!
//! Restore is an outage by design: every API server stops, every etcd stops,
//! each member is rebuilt from the same snapshot, etcd comes back and must
//! reach quorum, and only then do the API servers start again.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum BackupTarget {
    Local,
    CephVolume {
        name: String,
    },
    S3 {
        bucket: String,
        prefix: String,
        #[serde(default)]
        credentials_file: String,
        #[serde(default)]
        endpoint: String,
    },
}

/// Root directory for a Ceph-volume backup target (operator mounts the RBD here).
pub fn ceph_volume_backup_root(volume_name: &str) -> String {
    format!("/var/lib/kcore/kubernetes-backups-ceph/{volume_name}")
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BackupSpec {
    pub interval_secs: u64,
    pub retain_count: u32,
    pub retain_days: u32,
    pub target: BackupTarget,
}

pub fn sha256_hex(bytes: &[u8]) -> String {
    aws_lc_rs::digest::digest(&aws_lc_rs::digest::SHA256, bytes)
        .as_ref()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// Stored `backup_json`. Empty means backups are off.
pub fn parse_backup_spec(json: &str) -> Result<Option<BackupSpec>, String> {
    if json.trim().is_empty() {
        return Ok(None);
    }
    let spec: BackupSpec =
        serde_json::from_str(json).map_err(|e| format!("invalid backup policy: {e}"))?;
    spec.validate()?;
    Ok(Some(spec))
}

/// A snapshot is due when none exists or the newest is `interval_secs` old.
pub fn backup_due(newest_unix: Option<i64>, now: i64, interval_secs: u64) -> bool {
    match newest_unix {
        None => true,
        Some(at) => now.saturating_sub(at) >= interval_secs as i64,
    }
}

/// A snapshot taken at `taken_unix`, newest first by position.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SnapshotAge {
    pub id: String,
    pub taken_unix: i64,
}

/// Ids to delete. A snapshot survives while it is within `retain_count` newest
/// or younger than `retain_days`; a zero limit is not applied. `protected` ids
/// (for example the pre-upgrade snapshot) are never pruned.
pub fn snapshots_to_prune(
    newest_first: &[SnapshotAge],
    now: i64,
    retain_count: u32,
    retain_days: u32,
    protected: &[String],
) -> Vec<String> {
    newest_first
        .iter()
        .enumerate()
        .filter(|(index, snap)| {
            let within_count = retain_count > 0 && *index < retain_count as usize;
            let within_days = retain_days > 0
                && now.saturating_sub(snap.taken_unix) < retain_days as i64 * 86_400;
            !within_count && !within_days && !protected.contains(&snap.id)
        })
        .map(|(_, snap)| snap.id.clone())
        .collect()
}

impl BackupSpec {
    pub fn validate(&self) -> Result<(), String> {
        if self.interval_secs < 60 {
            return Err("backup interval must be at least 60 seconds".to_string());
        }
        if self.retain_count == 0 && self.retain_days == 0 {
            return Err(
                "set retain_count or retain_days; keeping nothing is not a backup".to_string(),
            );
        }
        match &self.target {
            BackupTarget::Local => {}
            BackupTarget::CephVolume { name } if name.trim().is_empty() => {
                return Err("ceph volume name is required".to_string());
            }
            BackupTarget::CephVolume { .. } => {}
            BackupTarget::S3 { bucket, .. } if bucket.trim().is_empty() => {
                return Err("s3 bucket is required".to_string());
            }
            BackupTarget::S3 {
                credentials_file, ..
            } if credentials_file.trim().is_empty() => {
                return Err("s3 credentials_file is required".to_string());
            }
            BackupTarget::S3 { .. } => {}
        }
        Ok(())
    }

    pub fn local_store_root(&self, default_local: &str) -> Result<String, String> {
        match &self.target {
            BackupTarget::Local => Ok(default_local.to_string()),
            BackupTarget::CephVolume { name } => {
                let root = ceph_volume_backup_root(name);
                if !std::path::Path::new(&root).is_dir() {
                    return Err(format!(
                        "ceph backup mount {root} is missing; mount the RBD volume there first"
                    ));
                }
                Ok(root)
            }
            BackupTarget::S3 { .. } => Ok(default_local.to_string()),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RestoreView {
    /// Control plane node ids, in node id order.
    pub members: Vec<String>,
    pub snapshot_available: bool,
    pub api_stopped: Vec<String>,
    pub etcd_stopped: Vec<String>,
    pub restored: Vec<String>,
    pub etcd_started: Vec<String>,
    pub etcd_quorum: bool,
    pub api_started: Vec<String>,
    pub api_ready: bool,
    pub failed: Option<String>,
    /// Snapshot id on the controller disk.
    #[serde(default)]
    pub snapshot_id: String,
    /// New etcd initial-cluster-token for this restore.
    #[serde(default)]
    pub cluster_token: String,
    /// `name=https://ip:2380,...` for etcdutl --initial-cluster.
    #[serde(default)]
    pub initial_cluster: String,
}

impl RestoreView {
    pub fn new(members: Vec<String>, snapshot_available: bool) -> Self {
        Self {
            members,
            snapshot_available,
            api_stopped: vec![],
            etcd_stopped: vec![],
            restored: vec![],
            etcd_started: vec![],
            etcd_quorum: false,
            api_started: vec![],
            api_ready: false,
            failed: None,
            snapshot_id: String::new(),
            cluster_token: String::new(),
            initial_cluster: String::new(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RestoreStep {
    StopApi { node_id: String },
    StopEtcd { node_id: String },
    RestoreMember { node_id: String },
    StartEtcd { node_id: String },
    WaitQuorum,
    StartApi { node_id: String },
    WaitApi,
    Done,
    Failed { reason: String },
}

fn first_missing<'a>(members: &'a [String], done: &[String]) -> Option<&'a String> {
    members.iter().find(|m| !done.contains(m))
}

pub fn next_restore_step(view: &RestoreView) -> RestoreStep {
    if let Some(reason) = &view.failed {
        return RestoreStep::Failed {
            reason: reason.clone(),
        };
    }
    if view.members.is_empty() {
        return RestoreStep::Failed {
            reason: "cluster has no control planes".to_string(),
        };
    }
    if !view.snapshot_available {
        return RestoreStep::Failed {
            reason: "no snapshot to restore from".to_string(),
        };
    }
    if let Some(node_id) = first_missing(&view.members, &view.api_stopped) {
        return RestoreStep::StopApi {
            node_id: node_id.clone(),
        };
    }
    if let Some(node_id) = first_missing(&view.members, &view.etcd_stopped) {
        return RestoreStep::StopEtcd {
            node_id: node_id.clone(),
        };
    }
    if let Some(node_id) = first_missing(&view.members, &view.restored) {
        return RestoreStep::RestoreMember {
            node_id: node_id.clone(),
        };
    }
    if let Some(node_id) = first_missing(&view.members, &view.etcd_started) {
        return RestoreStep::StartEtcd {
            node_id: node_id.clone(),
        };
    }
    if !view.etcd_quorum {
        return RestoreStep::WaitQuorum;
    }
    if let Some(node_id) = first_missing(&view.members, &view.api_started) {
        return RestoreStep::StartApi {
            node_id: node_id.clone(),
        };
    }
    if !view.api_ready {
        return RestoreStep::WaitApi;
    }
    RestoreStep::Done
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ages(items: &[(&str, i64)]) -> Vec<SnapshotAge> {
        items
            .iter()
            .map(|(id, at)| SnapshotAge {
                id: id.to_string(),
                taken_unix: *at,
            })
            .collect()
    }

    #[test]
    fn spec_round_trips_through_json() {
        let spec = BackupSpec {
            interval_secs: 3600,
            retain_count: 5,
            retain_days: 0,
            target: BackupTarget::Local,
        };
        let json = serde_json::to_string(&spec).unwrap();
        assert_eq!(parse_backup_spec(&json).unwrap(), Some(spec));
        assert_eq!(parse_backup_spec("").unwrap(), None);
        assert!(parse_backup_spec("{\"interval_secs\":5}").is_err());
    }

    #[test]
    fn backup_is_due_without_a_snapshot_or_after_the_interval() {
        assert!(backup_due(None, 1000, 600));
        assert!(!backup_due(Some(500), 1000, 600));
        assert!(backup_due(Some(400), 1000, 600));
    }

    #[test]
    fn prune_keeps_newest_count_and_protected() {
        let list = ages(&[("d", 400), ("c", 300), ("b", 200), ("a", 100)]);
        assert_eq!(snapshots_to_prune(&list, 1000, 2, 0, &[]), vec!["b", "a"]);
        assert_eq!(
            snapshots_to_prune(&list, 1000, 2, 0, &["a".to_string()]),
            vec!["b"]
        );
    }

    #[test]
    fn prune_by_days_keeps_young_snapshots() {
        let day = 86_400;
        let list = ages(&[("new", 10 * day), ("old", day)]);
        assert_eq!(snapshots_to_prune(&list, 10 * day, 0, 3, &[]), vec!["old"]);
        assert!(snapshots_to_prune(&list, 10 * day, 1, 3, &[]).contains(&"old".to_string()));
    }

    fn ids(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| s.to_string()).collect()
    }

    fn n(id: &str) -> String {
        id.to_string()
    }

    /// Applies each step like the orchestrator; waits resolve at once.
    fn drive(view: &mut RestoreView) -> Vec<RestoreStep> {
        let mut steps = Vec::new();
        for _ in 0..100 {
            let step = next_restore_step(view);
            match &step {
                RestoreStep::StopApi { node_id } => view.api_stopped.push(node_id.clone()),
                RestoreStep::StopEtcd { node_id } => view.etcd_stopped.push(node_id.clone()),
                RestoreStep::RestoreMember { node_id } => view.restored.push(node_id.clone()),
                RestoreStep::StartEtcd { node_id } => view.etcd_started.push(node_id.clone()),
                RestoreStep::WaitQuorum => view.etcd_quorum = true,
                RestoreStep::StartApi { node_id } => view.api_started.push(node_id.clone()),
                RestoreStep::WaitApi => view.api_ready = true,
                RestoreStep::Done | RestoreStep::Failed { .. } => {
                    steps.push(step);
                    return steps;
                }
            }
            steps.push(step);
        }
        panic!("restore did not settle");
    }

    #[test]
    fn full_restore_runs_in_order() {
        let mut view = RestoreView::new(ids(&["cp1", "cp2", "cp3"]), true);
        let steps = drive(&mut view);
        let mut expected = Vec::new();
        for id in ["cp1", "cp2", "cp3"] {
            expected.push(RestoreStep::StopApi { node_id: n(id) });
        }
        for id in ["cp1", "cp2", "cp3"] {
            expected.push(RestoreStep::StopEtcd { node_id: n(id) });
        }
        for id in ["cp1", "cp2", "cp3"] {
            expected.push(RestoreStep::RestoreMember { node_id: n(id) });
        }
        for id in ["cp1", "cp2", "cp3"] {
            expected.push(RestoreStep::StartEtcd { node_id: n(id) });
        }
        expected.push(RestoreStep::WaitQuorum);
        for id in ["cp1", "cp2", "cp3"] {
            expected.push(RestoreStep::StartApi { node_id: n(id) });
        }
        expected.push(RestoreStep::WaitApi);
        expected.push(RestoreStep::Done);
        assert_eq!(steps, expected);
    }

    #[test]
    fn single_control_plane_restore() {
        let mut view = RestoreView::new(ids(&["cp1"]), true);
        let steps = drive(&mut view);
        assert_eq!(steps.len(), 8);
        assert_eq!(steps.last(), Some(&RestoreStep::Done));
    }

    #[test]
    fn no_snapshot_fails() {
        let view = RestoreView::new(ids(&["cp1"]), false);
        assert!(matches!(
            next_restore_step(&view),
            RestoreStep::Failed { .. }
        ));
    }

    #[test]
    fn no_members_fails() {
        let view = RestoreView::new(vec![], true);
        assert!(matches!(
            next_restore_step(&view),
            RestoreStep::Failed { .. }
        ));
    }

    #[test]
    fn reported_failure_wins_over_progress() {
        let mut view = RestoreView::new(ids(&["cp1"]), true);
        view.failed = Some("etcdutl restore failed on cp1".to_string());
        assert_eq!(
            next_restore_step(&view),
            RestoreStep::Failed {
                reason: "etcdutl restore failed on cp1".to_string()
            }
        );
    }

    #[test]
    fn etcd_stops_only_after_every_api_stopped() {
        let mut view = RestoreView::new(ids(&["cp1", "cp2"]), true);
        view.api_stopped = ids(&["cp1"]);
        assert_eq!(
            next_restore_step(&view),
            RestoreStep::StopApi { node_id: n("cp2") }
        );
    }

    #[test]
    fn restore_waits_for_quorum_before_api() {
        let mut view = RestoreView::new(ids(&["cp1"]), true);
        view.api_stopped = ids(&["cp1"]);
        view.etcd_stopped = ids(&["cp1"]);
        view.restored = ids(&["cp1"]);
        view.etcd_started = ids(&["cp1"]);
        assert_eq!(next_restore_step(&view), RestoreStep::WaitQuorum);
        view.etcd_quorum = true;
        assert_eq!(
            next_restore_step(&view),
            RestoreStep::StartApi { node_id: n("cp1") }
        );
    }

    #[test]
    fn resumes_mid_restore() {
        let mut view = RestoreView::new(ids(&["cp1", "cp2"]), true);
        view.api_stopped = ids(&["cp1", "cp2"]);
        view.etcd_stopped = ids(&["cp1", "cp2"]);
        view.restored = ids(&["cp1"]);
        let steps = drive(&mut view);
        assert_eq!(
            steps.first(),
            Some(&RestoreStep::RestoreMember { node_id: n("cp2") })
        );
        assert_eq!(steps.last(), Some(&RestoreStep::Done));
    }

    fn spec(target: BackupTarget) -> BackupSpec {
        BackupSpec {
            interval_secs: 3600,
            retain_count: 7,
            retain_days: 0,
            target,
        }
    }

    #[test]
    fn valid_specs_pass() {
        assert!(spec(BackupTarget::Local).validate().is_ok());
        assert!(spec(BackupTarget::CephVolume { name: n("etcd") })
            .validate()
            .is_ok());
        assert!(spec(BackupTarget::S3 {
            bucket: n("b"),
            prefix: String::new(),
            credentials_file: n("/etc/kcore/s3.json"),
            endpoint: String::new(),
        })
        .validate()
        .is_ok());
    }

    #[test]
    fn short_interval_is_rejected() {
        let mut s = spec(BackupTarget::Local);
        s.interval_secs = 10;
        assert!(s.validate().is_err());
    }

    #[test]
    fn keeping_nothing_is_rejected() {
        let mut s = spec(BackupTarget::Local);
        s.retain_count = 0;
        s.retain_days = 0;
        assert!(s.validate().is_err());
    }

    #[test]
    fn empty_target_names_are_rejected() {
        assert!(spec(BackupTarget::CephVolume { name: n(" ") })
            .validate()
            .is_err());
        assert!(spec(BackupTarget::S3 {
            bucket: String::new(),
            prefix: n("p"),
            credentials_file: n("/etc/kcore/s3.json"),
            endpoint: String::new(),
        })
        .validate()
        .is_err());
        assert!(spec(BackupTarget::S3 {
            bucket: n("b"),
            prefix: String::new(),
            credentials_file: String::new(),
            endpoint: String::new(),
        })
        .validate()
        .is_err());
    }
}
