//! gRPC proto → dashboard DTO (testable without the network).

use crate::controller_client::controller_proto;
use crate::dto::{
    AccessControlEntryDto, AuditEventDto, CephClusterRowDto, ComplianceDto, HostInterfaceDto,
    LvmLogicalVolumeDto, LvmPhysicalVolumeDto, LvmVolumeGroupDto, NetworkOverviewDto,
    NetworkRowDto, NodeNetworkDto, NodeStorageDto, NodeSummaryDto, ObjectStoreRowDto,
    ReplicationConflictDto, ReplicationIncomingDto, ReplicationOutgoingDto, ReplicationStatusDto,
    SharedFilesystemRowDto, StorageDiskRowDto, StorageOverviewDto, VmOperationRowDto, VmRowDto,
    VmsPageDto, VolumeRowDto,
};
use crate::format::{self, paginate_by_name, VM_PAGE_SIZE};

pub fn compliance_from_proto(r: controller_proto::GetComplianceReportResponse) -> ComplianceDto {
    ComplianceDto {
        controller_version: r.controller_version,
        crypto_library: r.crypto_library,
        tls13_cipher_suites: r.tls13_cipher_suites,
        tls12_cipher_suites: r.tls12_cipher_suites,
        kx_groups: r.kx_groups,
        excluded_algorithms: r.excluded_algorithms,
        mtls_enabled: r.mtls_enabled,
        access_control: r
            .access_control
            .into_iter()
            .map(|e| AccessControlEntryDto {
                rpc_method: e.rpc_method,
                allowed_identities: e.allowed_identities,
            })
            .collect(),
        total_nodes: r.total_nodes,
        approved_nodes: r.approved_nodes,
        pending_nodes: r.pending_nodes,
        rejected_nodes: r.rejected_nodes,
        total_vms: r.total_vms,
        running_vms: r.running_vms,
        stopped_vms: r.stopped_vms,
        total_networks: r.total_networks,
        nat_networks: r.nat_networks,
        bridge_networks: r.bridge_networks,
        vxlan_networks: r.vxlan_networks,
        sub_ca_enabled: r.sub_ca_enabled,
        cert_auto_renewal_days: r.cert_auto_renewal_days,
        nodes_expiring_30d: r.nodes_expiring_30d,
        nodes_cert_unknown: r.nodes_cert_unknown,
        nodes: r
            .nodes
            .into_iter()
            .map(|n| NodeSummaryDto {
                node_id: n.node_id,
                hostname: n.hostname,
                address: n.address,
                dc_id: n.dc_id,
                approval_status: n.approval_status,
                cert_expiry_days: n.cert_expiry_days,
                luks_method: n.luks_method,
            })
            .collect(),
        nodes_luks_tpm2: r.nodes_luks_tpm2,
        nodes_luks_keyfile: r.nodes_luks_keyfile,
        nodes_luks_unknown: r.nodes_luks_unknown,
    }
}

pub fn vms_page_from_proto(vms: Vec<controller_proto::VmInfo>, page: u32) -> VmsPageDto {
    let rows: Vec<VmRowDto> = vms
        .into_iter()
        .map(|v| VmRowDto {
            id: v.id,
            name: v.name.clone(),
            state: format::vm_state_label(v.state).to_string(),
            cpu: v.cpu,
            memory: format::memory_mebibytes(v.memory_bytes),
            node_id: v.node_id,
            storage_backend: format::storage_backend_name(&v.storage_backend),
        })
        .collect();
    let page = page.max(1);
    let pv = paginate_by_name(rows, |r| r.name.clone(), page, VM_PAGE_SIZE);
    VmsPageDto {
        total_pages: pv.total_pages(),
        has_prev: pv.has_prev(),
        has_next: pv.has_next(),
        page: pv.page,
        page_size: pv.page_size,
        total: pv.total,
        vms: pv.items,
    }
}

fn classify_interface(name: &str) -> &'static str {
    if name == "lo" {
        "loopback"
    } else if name.starts_with("br-")
        || name.starts_with("kbr-")
        || name.starts_with("virbr")
        || name == "br0"
    {
        "bridge"
    } else if name.starts_with("tap-") || name.starts_with("vnet") {
        "tap"
    } else if name.contains('.') || name.starts_with("vlan") {
        "vlan"
    } else if name.starts_with("vxlan") || name.starts_with("kvx-") {
        "vxlan"
    } else if name.starts_with("eno")
        || name.starts_with("eth")
        || name.starts_with("enp")
        || name.starts_with("ens")
    {
        "physical"
    } else if name.starts_with("bond") {
        "bond"
    } else if name.starts_with("docker") || name.starts_with("veth") {
        "container"
    } else {
        "other"
    }
}

fn disk_role_hint(mountpoint: &str, fstype: &str) -> String {
    let mp = mountpoint.trim();
    let fs = fstype.trim();
    if mp == "/" {
        return "OS (root)".into();
    }
    if mp.starts_with("/boot") {
        return "OS (boot)".into();
    }
    if mp.starts_with("/nix") {
        return "OS (Nix store)".into();
    }
    if mp.contains("kcore") {
        return "Data (kcore)".into();
    }
    if !mp.is_empty() {
        return "Mounted".into();
    }
    if fs.eq_ignore_ascii_case("zfs") {
        return "ZFS component".into();
    }
    if fs.contains("LVM") || fs.eq_ignore_ascii_case("lvm2_member") {
        return "LVM PV".into();
    }
    if !fs.is_empty() {
        return "See partitions".into();
    }
    "Block device".into()
}

fn count_storage_backends(
    nodes: &[controller_proto::NodeStorageOverview],
) -> (i32, i32, i32, i32, i32) {
    let mut filesystem = 0;
    let mut lvm = 0;
    let mut zfs = 0;
    let mut ceph = 0;
    let mut unspecified = 0;
    for node in nodes {
        match node.storage_backend {
            x if x == controller_proto::StorageBackendType::Filesystem as i32 => filesystem += 1,
            x if x == controller_proto::StorageBackendType::Lvm as i32 => lvm += 1,
            x if x == controller_proto::StorageBackendType::Zfs as i32 => zfs += 1,
            x if x == controller_proto::StorageBackendType::Ceph as i32 => ceph += 1,
            _ => unspecified += 1,
        }
    }
    (filesystem, lvm, zfs, ceph, unspecified)
}

pub fn storage_overview_from_proto(
    r: controller_proto::GetStorageOverviewResponse,
) -> StorageOverviewDto {
    // Prefer the per-node enum so a controller that still folds Ceph into
    // "unspecified" still shows a Ceph count.
    let (filesystem, lvm, zfs, ceph, unspecified) = if r.nodes.is_empty() {
        (
            r.backend_filesystem_nodes,
            r.backend_lvm_nodes,
            r.backend_zfs_nodes,
            r.backend_ceph_nodes,
            r.backend_unspecified_nodes,
        )
    } else {
        count_storage_backends(&r.nodes)
    };
    StorageOverviewDto {
        approved_nodes: r.approved_nodes,
        nodes_disk_inventory_ok: r.nodes_disk_inventory_ok,
        backend_filesystem_nodes: filesystem,
        backend_lvm_nodes: lvm,
        backend_zfs_nodes: zfs,
        backend_ceph_nodes: ceph,
        backend_unspecified_nodes: unspecified,
        nodes_luks_tpm2: r.nodes_luks_tpm2,
        nodes_luks_keyfile: r.nodes_luks_keyfile,
        nodes_luks_unknown: r.nodes_luks_unknown,
        total_block_devices: r.total_block_devices,
        nodes: r
            .nodes
            .into_iter()
            .map(|n| NodeStorageDto {
                node_id: n.node_id,
                hostname: n.hostname,
                address: n.address,
                storage_backend: format::storage_backend_label(n.storage_backend).to_string(),
                luks_method: n.luks_method,
                disk_inventory_ok: n.disk_inventory_ok,
                disks: n
                    .disks
                    .into_iter()
                    .map(|d| {
                        let role_hint = disk_role_hint(&d.mountpoint, &d.fstype);
                        StorageDiskRowDto {
                            name: d.name,
                            path: d.path,
                            size: d.size,
                            model: d.model,
                            fstype: d.fstype,
                            mountpoint: d.mountpoint,
                            role_hint,
                        }
                    })
                    .collect(),
                lvm_inventory_ok: n.lvm_inventory_ok,
                lvm_volume_groups: n
                    .lvm_volume_groups
                    .into_iter()
                    .map(|vg| LvmVolumeGroupDto {
                        name: vg.name,
                        size: format::bytes_human(vg.size_bytes),
                        free: format::bytes_human(vg.free_bytes),
                        attr: vg.attr,
                    })
                    .collect(),
                lvm_logical_volumes: n
                    .lvm_logical_volumes
                    .into_iter()
                    .map(|lv| LvmLogicalVolumeDto {
                        name: lv.name,
                        vg_name: lv.vg_name,
                        size: format::bytes_human(lv.size_bytes),
                        attr: lv.attr,
                        path: lv.path,
                        pool: lv.pool,
                        data_percent: lv.data_percent,
                    })
                    .collect(),
                lvm_physical_volumes: n
                    .lvm_physical_volumes
                    .into_iter()
                    .map(|pv| LvmPhysicalVolumeDto {
                        name: pv.name,
                        vg_name: pv.vg_name,
                        size: format::bytes_human(pv.size_bytes),
                        free: format::bytes_human(pv.free_bytes),
                        attr: pv.attr,
                    })
                    .collect(),
            })
            .collect(),
    }
}

pub fn network_overview_from_proto(
    r: controller_proto::GetNetworkOverviewResponse,
) -> NetworkOverviewDto {
    NetworkOverviewDto {
        default_gateway_interface: r.default_gateway_interface,
        default_external_ip: r.default_external_ip,
        default_gateway_ip: r.default_gateway_ip,
        default_internal_netmask: r.default_internal_netmask,
        nodes: r
            .nodes
            .into_iter()
            .map(|n| NodeNetworkDto {
                node_id: n.node_id,
                hostname: n.hostname,
                address: n.address,
                gateway_interface: n.gateway_interface,
                disable_vxlan: n.disable_vxlan,
                interfaces: n
                    .interfaces
                    .into_iter()
                    .map(|i| {
                        let kind = classify_interface(&i.name).to_string();
                        HostInterfaceDto {
                            name: i.name,
                            mac_address: i.mac_address,
                            state: i.state,
                            mtu: i.mtu,
                            addresses: i.addresses,
                            kind,
                        }
                    })
                    .collect(),
            })
            .collect(),
    }
}

fn volume_vm_label(v: &controller_proto::VolumeInfo) -> String {
    if !v.vm_name.is_empty() {
        v.vm_name.clone()
    } else {
        v.vm_id.clone()
    }
}

pub fn volumes_from_proto(vols: Vec<controller_proto::VolumeInfo>) -> Vec<VolumeRowDto> {
    let mut rows: Vec<VolumeRowDto> = vols
        .into_iter()
        .map(|v| {
            let vm = volume_vm_label(&v);
            VolumeRowDto {
                name: v.name,
                role: v.role,
                attach_state: v.attach_state,
                size: format::bytes_human(v.storage_size_bytes),
                vm,
                encrypted: v.encrypted,
                backend: format::storage_backend_name(&v.storage_backend),
                pool: v.pool,
                node_id: v.node_id,
            }
        })
        .collect();
    rows.sort_by(|a, b| a.name.cmp(&b.name));
    rows
}

pub fn vm_operations_from_proto(ops: Vec<controller_proto::VmOperation>) -> Vec<VmOperationRowDto> {
    let mut rows: Vec<VmOperationRowDto> = ops
        .into_iter()
        .map(|o| VmOperationRowDto {
            id: o.id,
            kind: o.kind,
            phase: o.phase,
            vm: o.vm_id,
            cancel_requested: o.cancel_requested,
            send_succeeded: o.send_succeeded,
            source_node: o.source_node,
            target_node: o.target_node,
            started_at: timestamp_label(o.started_at),
            finished_at: timestamp_label(o.finished_at),
            detail: o.detail_json,
        })
        .collect();
    rows.sort_by(|a, b| b.id.cmp(&a.id));
    rows
}

pub fn networks_from_proto(nets: Vec<controller_proto::NetworkInfo>) -> Vec<NetworkRowDto> {
    let mut rows: Vec<NetworkRowDto> = nets
        .into_iter()
        .map(|n| NetworkRowDto {
            name: n.name.clone(),
            network_type: n.network_type,
            node_id: n.node_id,
            external_ip: n.external_ip,
            gateway_ip: n.gateway_ip,
            internal_netmask: n.internal_netmask,
            vlan_id: n.vlan_id,
            enable_outbound_nat: n.enable_outbound_nat,
            ipv6: format_ipv6(&n.ipv6_prefix, &n.ipv6_gateway),
            east_west_firewall: n.east_west_firewall,
            forwarded_ports: format_ports(&n.allowed_tcp_ports, &n.allowed_udp_ports),
        })
        .collect();
    rows.sort_by(|a, b| a.name.cmp(&b.name));
    rows
}

pub fn replication_status_from_proto(
    r: controller_proto::GetReplicationStatusResponse,
) -> ReplicationStatusDto {
    ReplicationStatusDto {
        outbox_head_event_id: r.outbox_head_event_id,
        outbox_size: r.outbox_size,
        outgoing: r
            .outgoing
            .into_iter()
            .map(|o| ReplicationOutgoingDto {
                peer_id: o.peer_id,
                last_acked_event_id: o.last_acked_event_id,
                lag_events: o.lag_events,
            })
            .collect(),
        incoming: r
            .incoming
            .into_iter()
            .map(|i| ReplicationIncomingDto {
                peer_endpoint: i.peer_endpoint,
                last_pulled_event_id: i.last_pulled_event_id,
                last_applied_event_id: i.last_applied_event_id,
            })
            .collect(),
        unresolved_conflicts: r.unresolved_conflicts,
        pending_compensation_jobs: r.pending_compensation_jobs,
        failed_compensation_jobs: r.failed_compensation_jobs,
        materialization_backlog: r.materialization_backlog,
        failed_reservations: r.failed_reservations,
        failed_retryable_reservations: r.failed_retryable_reservations,
        failed_non_retryable_reservations: r.failed_non_retryable_reservations,
        retry_exhausted_reservations: r.retry_exhausted_reservations,
        zero_manual_slo_healthy: r.zero_manual_slo_healthy,
        zero_manual_slo_violations: r.zero_manual_slo_violations,
    }
}

pub fn conflicts_from_proto(
    r: controller_proto::ListReplicationConflictsResponse,
) -> Vec<ReplicationConflictDto> {
    r.conflicts
        .into_iter()
        .map(|c| ReplicationConflictDto {
            id: c.id,
            resource_key: c.resource_key,
            incumbent_op_id: c.incumbent_op_id,
            challenger_op_id: c.challenger_op_id,
            incumbent_controller_id: c.incumbent_controller_id,
            challenger_controller_id: c.challenger_controller_id,
            reason: c.reason,
        })
        .collect()
}

fn timestamp_label(ts: Option<prost_types::Timestamp>) -> String {
    match ts {
        Some(t) if t.seconds > 0 => format::format_unix_utc(t.seconds),
        _ => "—".to_string(),
    }
}

fn format_ipv6(prefix: &str, gateway: &str) -> String {
    if prefix.is_empty() {
        "—".to_string()
    } else if gateway.is_empty() {
        prefix.to_string()
    } else {
        format!("{prefix} ({gateway})")
    }
}

fn format_ports(tcp: &[i32], udp: &[i32]) -> String {
    let mut parts = Vec::new();
    if !tcp.is_empty() {
        let list = tcp
            .iter()
            .map(|p| p.to_string())
            .collect::<Vec<_>>()
            .join(",");
        parts.push(format!("tcp {list}"));
    }
    if !udp.is_empty() {
        let list = udp
            .iter()
            .map(|p| p.to_string())
            .collect::<Vec<_>>()
            .join(",");
        parts.push(format!("udp {list}"));
    }
    if parts.is_empty() {
        "—".to_string()
    } else {
        parts.join(" ")
    }
}

fn join_nonempty(items: impl IntoIterator<Item = String>) -> String {
    let joined = items
        .into_iter()
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join(", ");
    if joined.is_empty() {
        "—".to_string()
    } else {
        joined
    }
}

pub fn ceph_clusters_from_proto(
    clusters: Vec<controller_proto::CephCluster>,
) -> Vec<CephClusterRowDto> {
    let mut rows: Vec<CephClusterRowDto> = clusters
        .into_iter()
        .map(|c| {
            let spec = c.spec.unwrap_or_default();
            let status = c.status.unwrap_or_default();
            let members = join_nonempty(spec.nodes.iter().map(|n| n.node_id.clone()));
            let osd_devices = join_nonempty(spec.nodes.iter().flat_map(|n| {
                let mut devs = vec![n.osd_device.clone()];
                devs.extend(n.osd_devices.clone());
                devs
            }));
            CephClusterRowDto {
                name: c.name,
                phase: format::service_phase_label(status.phase).to_string(),
                health_message: status.health_message,
                public_network: spec.public_network,
                cluster_network: spec.cluster_network,
                size: spec.size,
                min_size: spec.min_size,
                encrypt_osds: spec.encrypt_osds,
                members,
                osd_devices,
            }
        })
        .collect();
    rows.sort_by(|a, b| a.name.cmp(&b.name));
    rows
}

pub fn shared_filesystems_from_proto(
    filesystems: Vec<controller_proto::SharedFilesystem>,
) -> Vec<SharedFilesystemRowDto> {
    let mut rows: Vec<SharedFilesystemRowDto> = filesystems
        .into_iter()
        .map(|fs| {
            let spec = fs.spec.unwrap_or_default();
            let status = fs.status.unwrap_or_default();
            let quota = if spec.quota_bytes <= 0 {
                "—".to_string()
            } else {
                format::bytes_human(spec.quota_bytes)
            };
            SharedFilesystemRowDto {
                name: fs.name,
                ceph_cluster: spec.ceph_cluster,
                phase: format::service_phase_label(status.phase).to_string(),
                health_message: status.health_message,
                quota,
                clients: spec.clients.len() as i32,
            }
        })
        .collect();
    rows.sort_by(|a, b| a.name.cmp(&b.name));
    rows
}

pub fn object_stores_from_proto(
    stores: Vec<controller_proto::ObjectStore>,
) -> Vec<ObjectStoreRowDto> {
    let mut rows: Vec<ObjectStoreRowDto> = stores
        .into_iter()
        .map(|store| {
            let spec = store.spec.unwrap_or_default();
            let status = store.status.unwrap_or_default();
            ObjectStoreRowDto {
                name: store.name,
                ceph_cluster: spec.ceph_cluster,
                phase: format::service_phase_label(status.phase).to_string(),
                health_message: status.health_message,
                port: spec.port,
                tls: spec.tls,
                members: join_nonempty(spec.members),
            }
        })
        .collect();
    rows.sort_by(|a, b| a.name.cmp(&b.name));
    rows
}

pub fn filter_audit_events(
    events: Vec<AuditEventDto>,
    q: &str,
    action: &str,
    since: &str,
) -> Vec<AuditEventDto> {
    let q = q.trim().to_ascii_lowercase();
    let action = action.trim();
    let since = since.trim();
    events
        .into_iter()
        .filter(|event| {
            if !action.is_empty() && event.action != action {
                return false;
            }
            if !since.is_empty() && event.created_at.as_str() < since {
                return false;
            }
            if q.is_empty() {
                return true;
            }
            let hay = format!(
                "{} {} {} {}",
                event.actor, event.action, event.resource, event.detail
            )
            .to_ascii_lowercase();
            hay.contains(&q)
        })
        .collect()
}

pub fn filter_vm_operations(rows: Vec<VmOperationRowDto>, q: &str) -> Vec<VmOperationRowDto> {
    let q = q.trim().to_ascii_lowercase();
    if q.is_empty() {
        return rows;
    }
    rows.into_iter()
        .filter(|op| {
            format!(
                "{} {} {} {} {} {} {}",
                op.id, op.kind, op.phase, op.vm, op.source_node, op.target_node, op.detail
            )
            .to_ascii_lowercase()
            .contains(&q)
        })
        .collect()
}

pub fn audit_events_from_proto(events: Vec<controller_proto::AuditEvent>) -> Vec<AuditEventDto> {
    events
        .into_iter()
        .map(|e| AuditEventDto {
            id: e.id,
            actor: e.actor,
            action: e.action,
            resource: e.resource,
            created_at: e.created_at,
            detail: e.detail,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compliance_maps_core_fields() {
        let r = controller_proto::GetComplianceReportResponse {
            controller_version: "9.9.9".into(),
            crypto_library: "test-lib".into(),
            tls13_cipher_suites: vec!["A".into()],
            tls12_cipher_suites: vec!["B".into()],
            kx_groups: vec!["P-256".into()],
            excluded_algorithms: vec!["X".into()],
            mtls_enabled: true,
            access_control: vec![controller_proto::AccessControlEntry {
                rpc_method: "ListVms".into(),
                allowed_identities: "kctl".into(),
                required_operator_role: "read-only".into(),
            }],
            total_nodes: 2,
            approved_nodes: 1,
            pending_nodes: 1,
            rejected_nodes: 0,
            total_vms: 3,
            running_vms: 2,
            stopped_vms: 1,
            total_networks: 4,
            nat_networks: 2,
            bridge_networks: 1,
            vxlan_networks: 1,
            sub_ca_enabled: false,
            cert_auto_renewal_days: 30,
            nodes_expiring_30d: 0,
            nodes_cert_unknown: 0,
            nodes: vec![controller_proto::NodeInfo {
                node_id: "n1".into(),
                hostname: "h1".into(),
                address: "10.0.0.1:9091".into(),
                capacity: None,
                usage: None,
                status: "ready".into(),
                last_heartbeat: None,
                labels: vec![],
                storage_backend: 0,
                disable_vxlan: false,
                approval_status: "approved".into(),
                cert_expiry_days: 90,
                luks_method: "tpm2".into(),
                dc_id: "DC2".into(),
            }],
            nodes_luks_tpm2: 1,
            nodes_luks_keyfile: 0,
            nodes_luks_unknown: 0,
        };
        let d = compliance_from_proto(r);
        assert_eq!(d.controller_version, "9.9.9");
        assert_eq!(d.total_vms, 3);
        assert_eq!(d.nodes.len(), 1);
        assert_eq!(d.nodes[0].hostname, "h1");
        assert_eq!(d.nodes[0].dc_id, "DC2");
        assert_eq!(d.access_control[0].rpc_method, "ListVms");
    }

    #[test]
    fn vms_page_slices_and_sorts() {
        let vms = vec![
            controller_proto::VmInfo {
                id: "1".into(),
                name: "zebra".into(),
                state: 2,
                cpu: 2,
                memory_bytes: 1024 * 1024 * 1024,
                node_id: "n".into(),
                created_at: None,
                storage_backend: String::new(),
                storage_size_bytes: 0,
            },
            controller_proto::VmInfo {
                id: "2".into(),
                name: "alpha".into(),
                state: 1,
                cpu: 1,
                memory_bytes: 512 * 1024 * 1024,
                node_id: "n".into(),
                created_at: None,
                storage_backend: String::new(),
                storage_size_bytes: 0,
            },
        ];
        let p = vms_page_from_proto(vms, 1);
        assert_eq!(p.total, 2);
        assert_eq!(p.vms.len(), 2);
        assert_eq!(p.vms[0].name, "alpha");
        assert_eq!(p.vms[0].state, "Stopped");
        assert_eq!(p.vms[1].state, "Running");
    }

    #[test]
    fn volumes_maps_and_sorts_by_name() {
        let vols = vec![
            controller_proto::VolumeInfo {
                name: "vol-b".into(),
                role: "data".into(),
                attach_state: "attached".into(),
                storage_size_bytes: 10 * 1024 * 1024 * 1024,
                vm_name: "web".into(),
                vm_id: "vm-1".into(),
                encrypted: true,
                ..Default::default()
            },
            controller_proto::VolumeInfo {
                name: "vol-a".into(),
                role: "root".into(),
                attach_state: "detached".into(),
                storage_size_bytes: 20 * 1024 * 1024 * 1024,
                vm_name: String::new(),
                vm_id: "vm-orphan".into(),
                encrypted: false,
                ..Default::default()
            },
        ];
        let rows = volumes_from_proto(vols);
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].name, "vol-a");
        assert_eq!(rows[0].vm, "vm-orphan");
        assert!(!rows[0].encrypted);
        assert_eq!(rows[1].name, "vol-b");
        assert_eq!(rows[1].vm, "web");
        assert!(rows[1].encrypted);
        assert!(rows[1].size.contains("10"));
    }

    #[test]
    fn vm_operations_sort_newest_id_first() {
        let ops = vec![
            controller_proto::VmOperation {
                id: "op-001".into(),
                kind: "migrate".into(),
                phase: "running".into(),
                vm_id: "vm-a".into(),
                cancel_requested: false,
                send_succeeded: true,
                ..Default::default()
            },
            controller_proto::VmOperation {
                id: "op-999".into(),
                kind: "start".into(),
                phase: "done".into(),
                vm_id: "vm-b".into(),
                cancel_requested: true,
                send_succeeded: false,
                ..Default::default()
            },
        ];
        let rows = vm_operations_from_proto(ops);
        assert_eq!(rows[0].id, "op-999");
        assert_eq!(rows[1].id, "op-001");
        assert!(rows[0].cancel_requested);
    }

    #[test]
    fn networks_sort_by_name() {
        let nets = vec![
            controller_proto::NetworkInfo {
                name: "b-net".into(),
                external_ip: "".into(),
                gateway_ip: "".into(),
                internal_netmask: "".into(),
                node_id: "n".into(),
                allowed_tcp_ports: vec![],
                allowed_udp_ports: vec![],
                vlan_id: 0,
                network_type: "nat".into(),
                enable_outbound_nat: true,
                ..Default::default()
            },
            controller_proto::NetworkInfo {
                name: "a-net".into(),
                external_ip: "".into(),
                gateway_ip: "".into(),
                internal_netmask: "".into(),
                node_id: "n".into(),
                allowed_tcp_ports: vec![],
                allowed_udp_ports: vec![],
                vlan_id: 0,
                network_type: "bridge".into(),
                enable_outbound_nat: false,
                ..Default::default()
            },
        ];
        let rows = networks_from_proto(nets);
        assert_eq!(rows[0].name, "a-net");
        assert_eq!(rows[1].name, "b-net");
    }

    #[test]
    fn classify_interface_identifies_types() {
        assert_eq!(classify_interface("lo"), "loopback");
        assert_eq!(classify_interface("eno1"), "physical");
        assert_eq!(classify_interface("eth0"), "physical");
        assert_eq!(classify_interface("enp3s0"), "physical");
        assert_eq!(classify_interface("ens5"), "physical");
        assert_eq!(classify_interface("br-default"), "bridge");
        assert_eq!(classify_interface("kbr-net1"), "bridge");
        assert_eq!(classify_interface("virbr0"), "bridge");
        assert_eq!(classify_interface("br0"), "bridge");
        assert_eq!(classify_interface("tap-vm1"), "tap");
        assert_eq!(classify_interface("vnet0"), "tap");
        assert_eq!(classify_interface("eth0.100"), "vlan");
        assert_eq!(classify_interface("vlan42"), "vlan");
        assert_eq!(classify_interface("vxlan100"), "vxlan");
        assert_eq!(classify_interface("kvx-overlay"), "vxlan");
        assert_eq!(classify_interface("bond0"), "bond");
        assert_eq!(classify_interface("docker0"), "container");
        assert_eq!(classify_interface("veth1234"), "container");
        assert_eq!(classify_interface("wlan0"), "other");
    }

    #[test]
    fn network_overview_maps_and_classifies() {
        let r = controller_proto::GetNetworkOverviewResponse {
            default_gateway_interface: "eno1".into(),
            default_external_ip: "203.0.113.10".into(),
            default_gateway_ip: "10.0.0.1".into(),
            default_internal_netmask: "255.255.255.0".into(),
            nodes: vec![controller_proto::NodeNetworkInfo {
                node_id: "n1".into(),
                hostname: "host1".into(),
                address: "10.0.0.1:9443".into(),
                gateway_interface: "eno1".into(),
                disable_vxlan: false,
                interfaces: vec![
                    controller_proto::NetworkInterfaceDetail {
                        name: "eno1".into(),
                        mac_address: "aa:bb:cc:dd:ee:01".into(),
                        state: "UP".into(),
                        mtu: 1500,
                        addresses: vec!["10.0.0.1/24".into()],
                    },
                    controller_proto::NetworkInterfaceDetail {
                        name: "br-net".into(),
                        mac_address: "aa:bb:cc:dd:ee:02".into(),
                        state: "UP".into(),
                        mtu: 1500,
                        addresses: vec![],
                    },
                ],
            }],
        };
        let dto = network_overview_from_proto(r);
        assert_eq!(dto.default_external_ip, "203.0.113.10");
        assert_eq!(dto.nodes.len(), 1);
        assert_eq!(dto.nodes[0].interfaces.len(), 2);
        assert_eq!(dto.nodes[0].interfaces[0].kind, "physical");
        assert_eq!(dto.nodes[0].interfaces[1].kind, "bridge");
    }

    #[test]
    fn storage_overview_maps_nodes_and_hints() {
        let r = controller_proto::GetStorageOverviewResponse {
            approved_nodes: 1,
            nodes_disk_inventory_ok: 1,
            backend_filesystem_nodes: 1,
            backend_lvm_nodes: 0,
            backend_zfs_nodes: 0,
            backend_ceph_nodes: 0,
            backend_unspecified_nodes: 0,
            nodes_luks_tpm2: 0,
            nodes_luks_keyfile: 0,
            nodes_luks_unknown: 1,
            total_block_devices: 1,
            nodes: vec![controller_proto::NodeStorageOverview {
                node_id: "n1".into(),
                hostname: "h1".into(),
                address: "10.0.0.1:1".into(),
                storage_backend: controller_proto::StorageBackendType::Zfs as i32,
                luks_method: String::new(),
                disk_inventory_ok: true,
                disks: vec![controller_proto::StorageDiskDetail {
                    name: "sdx".into(),
                    path: "/dev/sdx".into(),
                    size: "1G".into(),
                    model: String::new(),
                    fstype: String::new(),
                    mountpoint: "/".into(),
                }],
                lvm_inventory_ok: true,
                lvm_volume_groups: vec![controller_proto::StorageLvmVolumeGroupDetail {
                    name: "vg-kcore".into(),
                    size_bytes: 500 * 1024 * 1024 * 1024,
                    free_bytes: 200 * 1024 * 1024 * 1024,
                    attr: "wz--n-".into(),
                }],
                lvm_logical_volumes: vec![controller_proto::StorageLvmLogicalVolumeDetail {
                    name: "thin-pool".into(),
                    vg_name: "vg-kcore".into(),
                    size_bytes: 300 * 1024 * 1024 * 1024,
                    attr: "twi-a-t---".into(),
                    path: "/dev/vg-kcore/thin-pool".into(),
                    pool: String::new(),
                    origin: String::new(),
                    data_percent: "42.00".into(),
                    metadata_percent: "1.20".into(),
                }],
                lvm_physical_volumes: vec![controller_proto::StorageLvmPhysicalVolumeDetail {
                    name: "/dev/sda3".into(),
                    vg_name: "vg-kcore".into(),
                    size_bytes: 500 * 1024 * 1024 * 1024,
                    free_bytes: 200 * 1024 * 1024 * 1024,
                    attr: "a--".into(),
                }],
            }],
        };
        let dto = storage_overview_from_proto(r);
        assert_eq!(dto.nodes.len(), 1);
        assert_eq!(dto.nodes[0].storage_backend, "ZFS");
        assert_eq!(dto.nodes[0].disks[0].role_hint, "OS (root)");
        assert!(dto.nodes[0].lvm_inventory_ok);
        assert_eq!(dto.nodes[0].lvm_volume_groups.len(), 1);
        assert_eq!(dto.nodes[0].lvm_volume_groups[0].name, "vg-kcore");
        assert!(dto.nodes[0].lvm_volume_groups[0].size.contains("500"));
        assert_eq!(dto.nodes[0].lvm_logical_volumes.len(), 1);
        assert_eq!(dto.nodes[0].lvm_logical_volumes[0].name, "thin-pool");
        assert_eq!(dto.nodes[0].lvm_logical_volumes[0].data_percent, "42.00");
        assert_eq!(dto.nodes[0].lvm_physical_volumes.len(), 1);
        assert_eq!(dto.nodes[0].lvm_physical_volumes[0].name, "/dev/sda3");
        assert_eq!(dto.backend_zfs_nodes, 1);
        assert_eq!(dto.backend_ceph_nodes, 0);
    }

    #[test]
    fn storage_overview_counts_ceph_even_when_aggregate_says_unspecified() {
        let r = controller_proto::GetStorageOverviewResponse {
            approved_nodes: 1,
            backend_unspecified_nodes: 1,
            nodes: vec![controller_proto::NodeStorageOverview {
                node_id: "n-ceph".into(),
                hostname: "ceph-1".into(),
                storage_backend: controller_proto::StorageBackendType::Ceph as i32,
                ..Default::default()
            }],
            ..Default::default()
        };
        let dto = storage_overview_from_proto(r);
        assert_eq!(dto.backend_ceph_nodes, 1);
        assert_eq!(dto.backend_unspecified_nodes, 0);
        assert_eq!(dto.nodes[0].storage_backend, "Ceph");
    }

    #[test]
    fn audit_search_filters_action_text_and_since() {
        let events = vec![
            AuditEventDto {
                id: 1,
                actor: "kctl".into(),
                action: "CreateVm".into(),
                resource: "vm/web-01".into(),
                created_at: "2026-08-04T12:00:00.000Z".into(),
                detail: String::new(),
            },
            AuditEventDto {
                id: 2,
                actor: "kcore-node-a".into(),
                action: "RegisterNode".into(),
                resource: "node/node-mock-a".into(),
                created_at: "2026-08-01T00:00:00.000Z".into(),
                detail: "joined".into(),
            },
        ];
        let by_text = filter_audit_events(events.clone(), "web-01", "", "");
        assert_eq!(by_text.len(), 1);
        assert_eq!(by_text[0].action, "CreateVm");
        let by_action = filter_audit_events(events.clone(), "", "RegisterNode", "");
        assert_eq!(by_action.len(), 1);
        assert_eq!(by_action[0].resource, "node/node-mock-a");
        let by_since = filter_audit_events(events, "", "", "2026-08-03");
        assert_eq!(by_since.len(), 1);
        assert_eq!(by_since[0].action, "CreateVm");
    }

    #[test]
    fn replication_status_maps_fields() {
        let r = controller_proto::GetReplicationStatusResponse {
            outbox_head_event_id: 10,
            outbox_size: 20,
            outgoing: vec![controller_proto::ReplicationOutgoingStatus {
                peer_id: "dc-west".into(),
                last_acked_event_id: 8,
                lag_events: 2,
            }],
            incoming: vec![controller_proto::ReplicationIncomingStatus {
                peer_endpoint: "10.0.1.1:9090".into(),
                last_pulled_event_id: 7,
                last_applied_event_id: 6,
            }],
            unresolved_conflicts: 1,
            pending_compensation_jobs: 2,
            failed_compensation_jobs: 3,
            materialization_backlog: 4,
            oldest_unresolved_conflict_age_seconds: 5,
            failed_reservations: 6,
            zero_manual_slo_healthy: false,
            zero_manual_slo_violations: vec!["failed_reservations=6".into()],
            failed_retryable_reservations: 7,
            failed_non_retryable_reservations: 8,
            retry_exhausted_reservations: 9,
        };
        let dto = replication_status_from_proto(r);
        assert_eq!(dto.outbox_head_event_id, 10);
        assert_eq!(dto.outbox_size, 20);
        assert_eq!(dto.outgoing.len(), 1);
        assert_eq!(dto.outgoing[0].peer_id, "dc-west");
        assert_eq!(dto.outgoing[0].lag_events, 2);
        assert_eq!(dto.incoming.len(), 1);
        assert_eq!(dto.incoming[0].peer_endpoint, "10.0.1.1:9090");
        assert_eq!(dto.unresolved_conflicts, 1);
        assert_eq!(dto.failed_retryable_reservations, 7);
        assert_eq!(dto.retry_exhausted_reservations, 9);
        assert!(!dto.zero_manual_slo_healthy);
    }

    #[test]
    fn conflicts_from_proto_maps_fields() {
        let r = controller_proto::ListReplicationConflictsResponse {
            conflicts: vec![controller_proto::ReplicationConflictInfo {
                id: 42,
                resource_key: "vm:abc".into(),
                incumbent_op_id: "op-1".into(),
                challenger_op_id: "op-2".into(),
                incumbent_controller_id: "ctrl-east".into(),
                challenger_controller_id: "ctrl-west".into(),
                reason: "concurrent create".into(),
            }],
        };
        let dtos = conflicts_from_proto(r);
        assert_eq!(dtos.len(), 1);
        assert_eq!(dtos[0].id, 42);
        assert_eq!(dtos[0].resource_key, "vm:abc");
        assert_eq!(dtos[0].reason, "concurrent create");
    }
}

/// Property-based tests (Phase 2) — pure DTO classifiers.
#[cfg(test)]
mod proptests {
    use super::{classify_interface, disk_role_hint};
    use proptest::prelude::*;

    proptest! {
        #![proptest_config(ProptestConfig {
            cases: 2_000,
            .. ProptestConfig::default()
        })]

        /// `classify_interface` never panics, and always returns one of
        /// the documented kind labels.
        #[test]
        fn classify_interface_known_set(name in ".{0,32}") {
            let kind = classify_interface(&name);
            prop_assert!(matches!(
                kind,
                "loopback"
                    | "bridge"
                    | "tap"
                    | "vlan"
                    | "vxlan"
                    | "physical"
                    | "bond"
                    | "container"
                    | "other"
            ));
        }

        /// Specific prefix-matching contracts encoded as properties so
        /// future refactors of `classify_interface` cannot silently
        /// reclassify production interfaces.
        #[test]
        fn classify_interface_prefix_contracts(suffix in "[a-z0-9_-]{0,8}") {
            prop_assert_eq!(classify_interface(&format!("kbr-{suffix}")), "bridge");
            prop_assert_eq!(classify_interface(&format!("br-{suffix}")), "bridge");
            prop_assert_eq!(classify_interface(&format!("tap-{suffix}")), "tap");
            prop_assert_eq!(classify_interface(&format!("eno{suffix}")), "physical");
            prop_assert_eq!(classify_interface(&format!("vxlan{suffix}")), "vxlan");
            prop_assert_eq!(classify_interface("lo"), "loopback");
        }

        /// `disk_role_hint` never panics on arbitrary input.
        #[test]
        fn disk_role_hint_never_panics(mp in ".{0,32}", fs in ".{0,16}") {
            let _ = disk_role_hint(&mp, &fs);
        }

        /// **Priority ordering**: a `/` mountpoint always wins, even
        /// when `fstype` would otherwise classify the disk.
        #[test]
        fn disk_role_hint_root_mountpoint_wins(fs in ".{0,16}") {
            prop_assert_eq!(disk_role_hint("/", &fs), "OS (root)");
        }

        /// `/boot*` mountpoints always classify as boot.
        #[test]
        fn disk_role_hint_boot_mountpoint(suffix in "[a-z0-9/_-]{0,16}", fs in ".{0,16}") {
            let mp = format!("/boot{suffix}");
            prop_assert_eq!(disk_role_hint(&mp, &fs), "OS (boot)");
        }

        /// Any mountpoint containing `kcore` classifies as data.
        #[test]
        fn disk_role_hint_kcore_data(prefix in "[/a-z0-9_-]{0,16}", suffix in "[a-z0-9_/-]{0,8}", fs in ".{0,16}") {
            let mp = format!("{prefix}kcore{suffix}");
            // Skip if the prefix happens to be exactly `/` or starts with `/boot` /
            // `/nix` — those branches fire first.
            prop_assume!(mp != "/");
            prop_assume!(!mp.starts_with("/boot"));
            prop_assume!(!mp.starts_with("/nix"));
            prop_assert_eq!(disk_role_hint(&mp, &fs), "Data (kcore)");
        }
    }
}
