//! VM high availability decisions.
//!
//! The controller calls these functions; none of them touch the network.
//! A protected VM is one the cluster can restart on another host: Ceph, or
//! ZFS with an asynchronous replica. Everything else is unprotected and is
//! labelled that way wherever a VM is listed.

use crate::db::{NodeRow, VmHaRow, VmRow};

pub const PRIORITY_HIGH: &str = "high";
pub const PRIORITY_MEDIUM: &str = "medium";
pub const PRIORITY_LOW: &str = "low";

pub const PROTECTION_PROTECTED: &str = "protected";
pub const PROTECTION_ASYNC: &str = "async";
pub const PROTECTION_UNPROTECTED: &str = "unprotected";

pub const ISOLATION_POWER_OFF: &str = "powerOff";
pub const ISOLATION_LEAVE_RUNNING: &str = "leaveRunning";

pub const CAPACITY_SUFFICIENT: &str = "sufficient";
pub const CAPACITY_INSUFFICIENT: &str = "insufficient";
pub const CAPACITY_DISABLED: &str = "disabled";
pub const CAPACITY_NOT_APPLICABLE: &str = "not-applicable";

/// Heartbeat period the node agent uses. The loss deadline must cover two
/// missed beats so one delayed heartbeat is not a host failure.
pub const NODE_HEARTBEAT_INTERVAL_SECS: i64 = 15;

/// Agent report once placement has masked moved Ceph guests and started the
/// confirmation target. Older agents never send it, so their fence stays.
pub const PLACEMENT_SETTLED: &str = "placement-settled";

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FenceRelease {
    /// No fence is held. A heartbeat by itself does not lift a blocklist.
    Keep,
    /// The fence stays. `vms` are the guests still waiting to be masked.
    Hold { vms: Vec<String> },
    /// Every held guest is masked or still assigned here, and placement settled.
    Unfence,
}

/// A returning host stays blocklisted until the agent reports
/// [`PLACEMENT_SETTLED`] and every held VM is either still assigned on that
/// host or named in a `placement-mask:` report. The first heartbeat after
/// boot arrives before the moved guest is masked.
pub fn fence_release(
    pending: bool,
    held: &[String],
    assigned: &[String],
    actions: &[String],
) -> FenceRelease {
    if !pending {
        return FenceRelease::Keep;
    }
    let settled = actions.iter().any(|action| action == PLACEMENT_SETTLED);
    let remaining: Vec<String> = held
        .iter()
        .filter(|vm| {
            let still_here = assigned.iter().any(|name| name == *vm);
            let masked = actions
                .iter()
                .any(|action| action == &format!("placement-mask:{vm}"));
            !still_here && !masked
        })
        .cloned()
        .collect();
    if remaining.is_empty() && settled {
        FenceRelease::Unfence
    } else {
        FenceRelease::Hold { vms: remaining }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VmPolicy {
    pub restart_priority: String,
    pub vm_monitoring: String,
    pub vm_monitoring_secs: i64,
    pub replica_node: String,
    pub replica_interval_secs: i64,
}

impl Default for VmPolicy {
    fn default() -> Self {
        Self {
            restart_priority: PRIORITY_MEDIUM.to_string(),
            vm_monitoring: "off".to_string(),
            vm_monitoring_secs: 120,
            replica_node: String::new(),
            replica_interval_secs: 0,
        }
    }
}

impl VmPolicy {
    pub fn normalized(mut self) -> Result<Self, String> {
        self.restart_priority = normalize_priority(&self.restart_priority)?;
        self.vm_monitoring = normalize_monitoring(&self.vm_monitoring)?;
        if self.vm_monitoring == "off" {
            self.vm_monitoring_secs = 0;
        } else if self.vm_monitoring_secs < 30 {
            return Err("vm monitoring restart interval must be at least 30 seconds".into());
        }
        self.replica_node = self.replica_node.trim().to_string();
        if self.replica_node.is_empty() {
            self.replica_interval_secs = 0;
        } else if !(60..=86_400).contains(&self.replica_interval_secs) {
            return Err("replica interval must be from 60 seconds through 1 day".into());
        }
        Ok(self)
    }
}

pub fn normalize_priority(raw: &str) -> Result<String, String> {
    let value = raw.trim().to_ascii_lowercase();
    let value = if value.is_empty() {
        PRIORITY_MEDIUM.to_string()
    } else {
        value
    };
    match value.as_str() {
        PRIORITY_HIGH | PRIORITY_MEDIUM | PRIORITY_LOW => Ok(value),
        _ => Err(format!(
            "restart priority must be high, medium, or low (got {raw})"
        )),
    }
}

pub fn normalize_monitoring(raw: &str) -> Result<String, String> {
    let value = raw.trim().to_ascii_lowercase();
    match value.as_str() {
        "" | "off" => Ok("off".to_string()),
        "restart" => Ok("restart".to_string()),
        _ => Err(format!("vm monitoring must be off or restart (got {raw})")),
    }
}

pub fn protection(storage_backend: &str, replica_node: &str) -> &'static str {
    match storage_backend {
        "ceph" => PROTECTION_PROTECTED,
        "zfs" if !replica_node.trim().is_empty() => PROTECTION_ASYNC,
        _ => PROTECTION_UNPROTECTED,
    }
}

pub fn is_protected(storage_backend: &str, replica_node: &str) -> bool {
    protection(storage_backend, replica_node) != PROTECTION_UNPROTECTED
}

/// Restart the guest on the new host when it was running, or when the
/// operator asked for it to start and we never learned otherwise.
pub fn should_restart(runtime_state: &str, auto_start: bool) -> bool {
    match runtime_state {
        "running" | "paused" => true,
        "stopped" => false,
        _ => auto_start,
    }
}

pub fn priority_rank(priority: &str) -> u8 {
    match priority {
        PRIORITY_HIGH => 0,
        PRIORITY_LOW => 2,
        _ => 1,
    }
}

/// High priority first, then larger memory, so a scarce host takes the
/// guests the operator marked and the ones that are hardest to place.
pub fn sort_for_restart(vms: &mut [VmRow], priority_of: impl Fn(&VmRow) -> String) {
    vms.sort_by(|a, b| {
        priority_rank(&priority_of(a))
            .cmp(&priority_rank(&priority_of(b)))
            .then(b.memory_bytes.cmp(&a.memory_bytes))
            .then(a.id.cmp(&b.id))
    });
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AdmissionHost {
    pub cpu: i64,
    pub memory: i64,
    /// This host can take restarted guests.
    pub survivor: bool,
    /// This host can fail and its protected guests must fit elsewhere.
    pub can_fail: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AdmissionVm {
    pub host_index: usize,
    pub cpu: i64,
    pub memory: i64,
    pub protected: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AdmissionReport {
    pub capacity: String,
    pub host_failures_tolerated: i32,
    pub memory_headroom_bytes: i64,
    pub cpu_headroom: i64,
    pub message: String,
}

/// `tolerate` host failures. A cluster with too few survivors to absorb that
/// many failures reports [`CAPACITY_NOT_APPLICABLE`] and does not refuse
/// placement: one host cannot reserve a spare copy of itself.
pub fn admission(hosts: &[AdmissionHost], vms: &[AdmissionVm], tolerate: i32) -> AdmissionReport {
    if tolerate <= 0 {
        return AdmissionReport {
            capacity: CAPACITY_DISABLED.to_string(),
            host_failures_tolerated: 0,
            memory_headroom_bytes: 0,
            cpu_headroom: 0,
            message: "admission control is off".into(),
        };
    }
    let survivors = hosts.iter().filter(|h| h.survivor).count();
    if survivors <= tolerate as usize {
        return AdmissionReport {
            capacity: CAPACITY_NOT_APPLICABLE.to_string(),
            host_failures_tolerated: 0,
            memory_headroom_bytes: 0,
            cpu_headroom: 0,
            message: format!(
                "admission control needs at least {} schedulable hosts to tolerate {tolerate}",
                tolerate + 1
            ),
        };
    }

    let mut reserved: Vec<(i64, i64)> = hosts.iter().map(|_| (0, 0)).collect();
    for vm in vms.iter().filter(|vm| vm.protected) {
        if let Some(slot) = reserved.get_mut(vm.host_index) {
            slot.0 += vm.cpu;
            slot.1 += vm.memory;
        }
    }

    let mut failing: Vec<usize> = (0..hosts.len())
        .filter(|&i| hosts[i].can_fail || hosts[i].survivor)
        .collect();
    failing.sort_by(|&a, &b| {
        reserved[b]
            .1
            .cmp(&reserved[a].1)
            .then(reserved[b].0.cmp(&reserved[a].0))
            .then(a.cmp(&b))
    });
    let lose = tolerate as usize;
    let lost: Vec<usize> = failing.into_iter().take(lose).collect();
    let lost_cpu: i64 = lost.iter().map(|&i| reserved[i].0).sum();
    let lost_mem: i64 = lost.iter().map(|&i| reserved[i].1).sum();

    let mut free_cpu = 0_i64;
    let mut free_mem = 0_i64;
    for (i, host) in hosts.iter().enumerate() {
        if !host.survivor || lost.contains(&i) {
            continue;
        }
        free_cpu += (host.cpu - reserved[i].0).max(0);
        free_mem += (host.memory - reserved[i].1).max(0);
    }
    let mem_headroom = free_mem - lost_mem;
    let cpu_headroom = free_cpu - lost_cpu;
    if mem_headroom >= 0 && cpu_headroom >= 0 {
        AdmissionReport {
            capacity: CAPACITY_SUFFICIENT.to_string(),
            host_failures_tolerated: tolerate,
            memory_headroom_bytes: mem_headroom,
            cpu_headroom,
            message: format!("can tolerate {tolerate} host failure(s)"),
        }
    } else {
        AdmissionReport {
            capacity: CAPACITY_INSUFFICIENT.to_string(),
            host_failures_tolerated: 0,
            memory_headroom_bytes: mem_headroom,
            cpu_headroom,
            message: format!(
                "losing {tolerate} host(s) leaves a shortfall of {} bytes and {cpu_headroom} cores",
                -mem_headroom.min(0)
            ),
        }
    }
}

pub fn host_role(node: &NodeRow) -> (bool, bool) {
    let approved = node.approval_status == "approved";
    let survivor = approved && node.status == "ready";
    let can_fail = approved && matches!(node.status.as_str(), "ready" | "cordoned" | "draining");
    (survivor, can_fail)
}

/// Seconds of guest data a ZFS replica would lose if the primary died now.
pub fn replica_rpo_secs(last_replica_unix: i64, now_unix: i64) -> i64 {
    if last_replica_unix <= 0 {
        return -1;
    }
    (now_unix - last_replica_unix).max(0)
}

pub fn replica_due(last_replica_unix: i64, interval_secs: i64, now_unix: i64) -> bool {
    interval_secs > 0 && (last_replica_unix <= 0 || now_unix - last_replica_unix >= interval_secs)
}

pub fn monitor_due(
    now_unix: i64,
    last_guest_heartbeat: i64,
    restart_after_secs: i64,
    last_restart_unix: i64,
) -> bool {
    if restart_after_secs < 30 || last_guest_heartbeat <= 0 {
        return false;
    }
    if last_restart_unix > 0 && now_unix - last_restart_unix < restart_after_secs {
        return false;
    }
    now_unix - last_guest_heartbeat >= restart_after_secs
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LeaseView {
    pub holder: String,
    pub expires_at_ms: i64,
    pub term: i64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LeaseDecision {
    /// This controller may fence and move VMs. `term` is what to persist.
    Act { term: i64, expires_at_ms: i64 },
    /// Someone else holds the lease, or this controller is too far behind.
    Wait,
}

/// One controller moves VMs. A controller with no replication peers always
/// acts. A peer acts only with a fresh direct heartbeat (so a replica that
/// has not heard from a node cannot declare it dead) and an apply lag under
/// `max_lag_ms`.
pub fn decide_lease(
    now_ms: i64,
    self_id: &str,
    peers_configured: bool,
    direct_heartbeat_fresh: bool,
    apply_lag_ms: i64,
    max_lag_ms: i64,
    renew_ms: i64,
    current: Option<&LeaseView>,
) -> LeaseDecision {
    let renew_ms = renew_ms.max(1);
    let expires = now_ms.saturating_add(renew_ms.saturating_mul(3));
    if !peers_configured {
        return LeaseDecision::Act {
            term: current.map(|l| l.term).unwrap_or(1),
            expires_at_ms: expires,
        };
    }
    if !direct_heartbeat_fresh || apply_lag_ms > max_lag_ms {
        return LeaseDecision::Wait;
    }
    match current {
        Some(lease) if lease.expires_at_ms > now_ms && lease.holder != self_id => {
            LeaseDecision::Wait
        }
        Some(lease) if lease.expires_at_ms > now_ms && lease.holder == self_id => {
            LeaseDecision::Act {
                term: lease.term,
                expires_at_ms: expires,
            }
        }
        other => LeaseDecision::Act {
            term: other.map(|l| l.term.saturating_add(1)).unwrap_or(1),
            expires_at_ms: expires,
        },
    }
}

pub fn now_unix_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX))
        .unwrap_or(0)
}

pub fn now_unix_secs() -> i64 {
    now_unix_ms() / 1000
}

pub fn policy_from_row(row: &VmHaRow) -> VmPolicy {
    VmPolicy {
        restart_priority: row.restart_priority.clone(),
        vm_monitoring: row.vm_monitoring.clone(),
        vm_monitoring_secs: row.vm_monitoring_secs,
        replica_node: row.replica_node.clone(),
        replica_interval_secs: row.replica_interval_secs,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn host(cpu: i64, memory: i64, survivor: bool, can_fail: bool) -> AdmissionHost {
        AdmissionHost {
            cpu,
            memory,
            survivor,
            can_fail,
        }
    }

    fn vm(host_index: usize, cpu: i64, memory: i64) -> AdmissionVm {
        AdmissionVm {
            host_index,
            cpu,
            memory,
            protected: true,
        }
    }

    #[test]
    fn protection_follows_storage() {
        assert_eq!(protection("ceph", ""), PROTECTION_PROTECTED);
        assert_eq!(protection("zfs", "node-b"), PROTECTION_ASYNC);
        assert_eq!(protection("zfs", ""), PROTECTION_UNPROTECTED);
        assert_eq!(protection("lvm", ""), PROTECTION_UNPROTECTED);
        assert_eq!(protection("filesystem", "node-b"), PROTECTION_UNPROTECTED);
    }

    #[test]
    fn restart_follows_what_was_running() {
        assert!(should_restart("running", false));
        assert!(should_restart("paused", false));
        assert!(!should_restart("stopped", true));
        assert!(should_restart("unknown", true));
        assert!(!should_restart("unknown", false));
    }

    #[test]
    fn restart_order_is_priority_then_memory() {
        let mut vms = vec![
            row("low-big", PRIORITY_LOW, 8),
            row("high-small", PRIORITY_HIGH, 1),
            row("med", PRIORITY_MEDIUM, 4),
            row("high-big", PRIORITY_HIGH, 9),
        ];
        sort_for_restart(&mut vms, |vm| vm.image_path.clone());
        let names: Vec<_> = vms.iter().map(|vm| vm.name.as_str()).collect();
        assert_eq!(names, ["high-big", "high-small", "med", "low-big"]);
    }

    #[test]
    fn admission_reserves_the_largest_hosts() {
        let hosts = vec![
            host(8, 100, true, true),
            host(8, 100, true, true),
            host(8, 100, true, true),
        ];
        let vms = vec![vm(0, 2, 40), vm(1, 2, 30), vm(2, 1, 10)];
        let report = admission(&hosts, &vms, 1);
        assert_eq!(report.capacity, CAPACITY_SUFFICIENT);
        // Losing the 40-byte host leaves  (100-30) + (100-10) - 40 = 120.
        assert_eq!(report.memory_headroom_bytes, 120);
    }

    #[test]
    fn admission_refuses_a_placement_that_cannot_be_absorbed() {
        // Each host is too full for the other to take its guests. An exact
        // fit on an empty survivor is allowed; this is the case that is not.
        let hosts = vec![host(4, 50, true, true), host(4, 50, true, true)];
        let vms = vec![vm(0, 4, 40), vm(1, 2, 40)];
        let report = admission(&hosts, &vms, 1);
        assert_eq!(report.capacity, CAPACITY_INSUFFICIENT);
        assert!(report.memory_headroom_bytes < 0, "{report:?}");
    }

    #[test]
    fn admission_does_not_apply_to_a_single_host() {
        let hosts = vec![host(4, 50, true, true)];
        let report = admission(&hosts, &[vm(0, 4, 50)], 1);
        assert_eq!(report.capacity, CAPACITY_NOT_APPLICABLE);
    }

    #[test]
    fn admission_ignores_unprotected_guests() {
        let hosts = vec![host(4, 50, true, true), host(4, 50, true, true)];
        let mut local = vm(0, 4, 50);
        local.protected = false;
        let report = admission(&hosts, &[local], 1);
        assert_eq!(report.capacity, CAPACITY_SUFFICIENT);
        assert_eq!(report.memory_headroom_bytes, 50);
    }

    #[test]
    fn a_cordoned_host_can_fail_but_cannot_take_guests() {
        let hosts = vec![
            host(8, 100, false, true),
            host(8, 100, true, true),
            host(8, 100, true, true),
        ];
        let vms = vec![vm(0, 2, 80)];
        let report = admission(&hosts, &vms, 1);
        assert_eq!(report.capacity, CAPACITY_SUFFICIENT);
        assert_eq!(report.memory_headroom_bytes, 200 - 80);
    }

    #[test]
    fn replica_and_monitor_clocks() {
        assert!(replica_due(0, 300, 1_000));
        assert!(!replica_due(900, 300, 1_000));
        assert!(replica_due(700, 300, 1_000));
        assert_eq!(replica_rpo_secs(800, 1_000), 200);
        assert_eq!(replica_rpo_secs(0, 1_000), -1);
        assert!(!monitor_due(1_000, 0, 120, 0));
        assert!(monitor_due(1_000, 800, 120, 0));
        assert!(!monitor_due(1_000, 800, 120, 950));
        assert!(!monitor_due(1_000, 950, 120, 0));
    }

    #[test]
    fn lease_is_single_writer_and_steps_down_when_stale() {
        let held = LeaseView {
            holder: "a".into(),
            expires_at_ms: 10_000,
            term: 3,
        };
        assert!(matches!(
            decide_lease(5_000, "b", true, true, 0, 1_000, 1_000, Some(&held)),
            LeaseDecision::Wait
        ));
        match decide_lease(5_000, "a", true, true, 0, 1_000, 1_000, Some(&held)) {
            LeaseDecision::Act { term, .. } => assert_eq!(term, 3),
            LeaseDecision::Wait => panic!("holder should renew"),
        }
        match decide_lease(11_000, "b", true, true, 0, 1_000, 1_000, Some(&held)) {
            LeaseDecision::Act { term, .. } => assert_eq!(term, 4),
            LeaseDecision::Wait => panic!("expired lease should be claimable"),
        }
        assert!(matches!(
            decide_lease(11_000, "b", true, false, 0, 1_000, 1_000, Some(&held)),
            LeaseDecision::Wait
        ));
        assert!(matches!(
            decide_lease(11_000, "b", true, true, 5_000, 1_000, 1_000, Some(&held)),
            LeaseDecision::Wait
        ));
        assert!(matches!(
            decide_lease(1, "solo", false, false, 9_000, 1, 1_000, None),
            LeaseDecision::Act { .. }
        ));
    }

    #[test]
    fn a_returning_host_stays_fenced_until_the_moved_vm_is_masked() {
        let held = vec!["labtest-9".to_string()];
        assert_eq!(
            fence_release(false, &held, &[], &["placement-settled".into()]),
            FenceRelease::Keep
        );
        assert_eq!(
            fence_release(true, &held, &[], &[]),
            FenceRelease::Hold {
                vms: vec!["labtest-9".into()]
            }
        );
        assert_eq!(
            fence_release(true, &held, &[], &["placement-settled".into()]),
            FenceRelease::Hold {
                vms: vec!["labtest-9".into()]
            }
        );
        assert_eq!(
            fence_release(
                true,
                &held,
                &[],
                &[
                    "placement-mask:labtest-9".into(),
                    "placement-settled".into()
                ]
            ),
            FenceRelease::Unfence
        );
        // The move failed, so the guest still belongs here. Unfence waits
        // until placement has settled and the unit is gated.
        assert_eq!(
            fence_release(true, &held, &["labtest-9".into()], &[]),
            FenceRelease::Hold { vms: vec![] }
        );
        assert_eq!(
            fence_release(
                true,
                &held,
                &["labtest-9".into()],
                &["placement-settled".into()]
            ),
            FenceRelease::Unfence
        );
        assert_eq!(
            fence_release(
                true,
                &["labtest-9".into(), "other".into()],
                &[],
                &[
                    "placement-mask:labtest-9".into(),
                    "placement-settled".into()
                ]
            ),
            FenceRelease::Hold {
                vms: vec!["other".into()]
            }
        );
    }

    #[test]
    fn policy_rejects_unknown_priority_and_a_short_replica() {
        let bad = VmPolicy {
            restart_priority: "urgent".into(),
            ..VmPolicy::default()
        };
        assert!(bad.normalized().is_err());
        let replica = VmPolicy {
            replica_node: "node-b".into(),
            replica_interval_secs: 10,
            ..VmPolicy::default()
        };
        assert!(replica.normalized().is_err());
        let ok = VmPolicy {
            restart_priority: "HIGH".into(),
            replica_node: "node-b".into(),
            replica_interval_secs: 300,
            vm_monitoring: "restart".into(),
            vm_monitoring_secs: 90,
        };
        let ok = ok.normalized().unwrap();
        assert_eq!(ok.restart_priority, "high");
        assert_eq!(ok.vm_monitoring, "restart");
    }

    fn row(name: &str, priority: &str, memory_gib: i64) -> VmRow {
        VmRow {
            id: name.into(),
            name: name.into(),
            cpu: 1,
            memory_bytes: memory_gib * 1024 * 1024 * 1024,
            image_path: priority.into(),
            image_url: String::new(),
            image_sha256: String::new(),
            image_format: "raw".into(),
            image_size: 0,
            network: "default".into(),
            auto_start: true,
            node_id: "n".into(),
            created_at: String::new(),
            runtime_state: "running".into(),
            cloud_init_user_data: String::new(),
            storage_backend: "ceph".into(),
            storage_size_bytes: 0,
            vm_ip: String::new(),
            pci_devices: String::new(),
            labels: String::new(),
        }
    }
}
