//! Cluster health is derived from nodes, VM placement, certificate expiry,
//! and unresolved replication conflicts. It does not probe nodes itself.

use crate::db::{NodeRow, VmRow};

/// Node certificates at or below this many days are reported as expiring.
pub const CERT_EXPIRY_WARN_DAYS: i32 = 30;

pub const STATUS_HEALTHY: &str = "healthy";
pub const STATUS_DEGRADED: &str = "degraded";
pub const STATUS_UNHEALTHY: &str = "unhealthy";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClusterHealth {
    pub status: String,
    pub ready_nodes: i32,
    pub not_ready_nodes: i32,
    pub unschedulable_nodes: i32,
    pub pending_nodes: i32,
    pub vm_count: i32,
    pub vms_on_not_ready_nodes: i32,
    pub replication_conflicts: i32,
    pub certificates_expiring_soon: i32,
    pub conditions: Vec<String>,
}

pub fn assess(
    nodes: &[NodeRow],
    vms: &[VmRow],
    replication_conflicts: i64,
    inventory_expiring_soon: i32,
) -> ClusterHealth {
    let mut ready_nodes = 0;
    let mut not_ready_nodes = 0;
    let mut unschedulable_nodes = 0;
    let mut pending_nodes = 0;
    let mut node_certs_expiring = 0;
    let mut not_ready_ids = Vec::new();

    for node in nodes {
        if node.approval_status == "pending" {
            pending_nodes += 1;
        }
        if node.approval_status == "approved" && node.status == "ready" {
            ready_nodes += 1;
        }
        if node.approval_status == "approved" && node.status == "not-ready" {
            not_ready_nodes += 1;
            not_ready_ids.push(node.id.as_str());
        }
        if matches!(node.status.as_str(), "cordoned" | "draining" | "drained") {
            unschedulable_nodes += 1;
        }
        if (0..=CERT_EXPIRY_WARN_DAYS).contains(&node.cert_expiry_days) {
            node_certs_expiring += 1;
        }
    }

    let vms_on_not_ready_nodes = vms
        .iter()
        .filter(|vm| not_ready_ids.iter().any(|id| *id == vm.node_id))
        .count() as i32;
    let certificates_expiring_soon = node_certs_expiring.max(inventory_expiring_soon);

    let mut conditions = Vec::new();
    if ready_nodes == 0 {
        conditions.push("no approved node is ready".to_string());
    }
    if not_ready_nodes > 0 {
        conditions.push(format!(
            "{not_ready_nodes} approved node(s) missed the heartbeat deadline"
        ));
    }
    if vms_on_not_ready_nodes > 0 {
        conditions.push(format!(
            "{vms_on_not_ready_nodes} VM(s) still record a not-ready node"
        ));
    }
    if unschedulable_nodes > 0 {
        conditions.push(format!(
            "{unschedulable_nodes} node(s) are cordoned, draining, or drained"
        ));
    }
    if pending_nodes > 0 {
        conditions.push(format!("{pending_nodes} node(s) are pending approval"));
    }
    if replication_conflicts > 0 {
        conditions.push(format!(
            "{replication_conflicts} unresolved replication conflict(s)"
        ));
    }
    if certificates_expiring_soon > 0 {
        conditions.push(format!(
            "{certificates_expiring_soon} certificate(s) expire within {CERT_EXPIRY_WARN_DAYS} days"
        ));
    }

    let status = if ready_nodes == 0 {
        STATUS_UNHEALTHY
    } else if conditions.is_empty() {
        STATUS_HEALTHY
    } else {
        STATUS_DEGRADED
    };

    ClusterHealth {
        status: status.to_string(),
        ready_nodes,
        not_ready_nodes,
        unschedulable_nodes,
        pending_nodes,
        vm_count: vms.len() as i32,
        vms_on_not_ready_nodes,
        replication_conflicts: i32::try_from(replication_conflicts).unwrap_or(i32::MAX),
        certificates_expiring_soon,
        conditions,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn node(id: &str, status: &str, approval: &str) -> NodeRow {
        NodeRow {
            id: id.to_string(),
            hostname: id.to_string(),
            address: "127.0.0.1:9091".to_string(),
            cpu_cores: 4,
            memory_bytes: 1024,
            status: status.to_string(),
            last_heartbeat: String::new(),
            gateway_interface: String::new(),
            cpu_used: 0,
            memory_used: 0,
            storage_backend: "filesystem".to_string(),
            disable_vxlan: false,
            approval_status: approval.to_string(),
            cert_expiry_days: -1,
            luks_method: String::new(),
            dc_id: "DC1".to_string(),
        }
    }

    fn vm(node_id: &str) -> VmRow {
        VmRow {
            id: format!("vm-{node_id}"),
            name: format!("vm-{node_id}"),
            cpu: 1,
            memory_bytes: 1024,
            image_path: String::new(),
            image_url: String::new(),
            image_sha256: String::new(),
            image_format: "raw".to_string(),
            image_size: 0,
            network: "default".to_string(),
            auto_start: true,
            node_id: node_id.to_string(),
            created_at: String::new(),
            runtime_state: "running".to_string(),
            cloud_init_user_data: String::new(),
            storage_backend: "filesystem".to_string(),
            storage_size_bytes: 0,
            vm_ip: String::new(),
            pci_devices: String::new(),
        }
    }

    #[test]
    fn empty_cluster_is_unhealthy() {
        let health = assess(&[], &[], 0, 0);
        assert_eq!(health.status, STATUS_UNHEALTHY);
        assert!(health.conditions.iter().any(|c| c.contains("no approved")));
    }

    #[test]
    fn one_ready_node_is_healthy() {
        let health = assess(&[node("n1", "ready", "approved")], &[], 0, 0);
        assert_eq!(health.status, STATUS_HEALTHY);
        assert!(health.conditions.is_empty());
        assert_eq!(health.ready_nodes, 1);
    }

    #[test]
    fn not_ready_node_with_a_vm_is_degraded() {
        let health = assess(
            &[
                node("n1", "ready", "approved"),
                node("n2", "not-ready", "approved"),
            ],
            &[vm("n2")],
            0,
            0,
        );
        assert_eq!(health.status, STATUS_DEGRADED);
        assert_eq!(health.not_ready_nodes, 1);
        assert_eq!(health.vms_on_not_ready_nodes, 1);
        assert_eq!(health.vm_count, 1);
    }

    #[test]
    fn cordon_and_expiring_certificate_are_degraded() {
        let mut cordoned = node("n2", "cordoned", "approved");
        cordoned.cert_expiry_days = 10;
        let health = assess(&[node("n1", "ready", "approved"), cordoned], &[], 1, 0);
        assert_eq!(health.status, STATUS_DEGRADED);
        assert_eq!(health.unschedulable_nodes, 1);
        assert_eq!(health.certificates_expiring_soon, 1);
        assert_eq!(health.replication_conflicts, 1);
    }
}
