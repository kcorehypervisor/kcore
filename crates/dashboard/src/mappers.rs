//! gRPC proto → dashboard DTO (testable without the network).

use crate::ceph_status::{self, capacity_dto, capacity_from_status_json, health_from_status_json};
use crate::controller_client::controller_proto;
use crate::dto::{
    AccessControlEntryDto, AuditEventDto, CephClusterDetailDto, CephClusterRowDto,
    CephDeviceClassDto, CephMemberDto, CephPoolUsageDto, CertificateRowDto, ClusterHealthDto,
    ComplianceDto, ContainerDetailDto, ContainerRowDto, CrlSummaryDto, CryptoConfigDto,
    DbmsDetailDto, DbmsRowDto, DeviceDto, DiskLayoutDetailDto, DiskLayoutRowDto, HostInterfaceDto,
    KubernetesClusterDetailDto, KubernetesClusterRowDto, KubernetesHostVolumeDto,
    KubernetesNodeDto, KubernetesNodeHealthDto, LvmLogicalVolumeDto, LvmPhysicalVolumeDto,
    LvmVolumeGroupDto, MigrateNodeDto, NetworkOverviewDto, NetworkRowDto, NodeDetailDto,
    NodeNetworkDto, NodeRowDto, NodeStorageDto, NodeSummaryDto, ObjectStoreDetailDto,
    ObjectStoreRowDto, OperationDetailDto, PkiStatusDto, ReplicationConflictDto,
    ReplicationIncomingDto, ReplicationOutgoingDto, ReplicationStatusDto, SbomSummaryDto,
    SharedFilesystemDetailDto, SharedFilesystemRowDto, SnapshotPolicyDto, SnapshotRowDto,
    SshKeyDetailDto, SshKeyRowDto, StorageClassDetailDto, StorageClassDto, StorageDiskRowDto,
    StorageOverviewDto, VmDetailDto, VmDiskDto, VmNicDto, VmOperationRowDto, VmRowDto, VmsPageDto,
    VolumeDetailDto, VolumeRowDto,
};
use crate::format::{self, paginate_by_name};

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

pub fn vms_page_from_proto(
    vms: Vec<controller_proto::VmInfo>,
    page: u32,
    page_size: usize,
) -> VmsPageDto {
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
    let page_size = format::normalize_page_size(page_size as u32);
    let pv = paginate_by_name(rows, |r| r.name.clone(), page, page_size);
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
    sort_volume_rows(&mut rows);
    rows
}

/// Registered volumes plus LVM logical volumes reported by node inventory.
///
/// ZFS rows are the controller volumes whose backend is ZFS. The storage
/// overview has no zvol list, so an empty ZFS section means none are registered.
pub fn volumes_with_host_inventory(
    vols: Vec<controller_proto::VolumeInfo>,
    storage: Option<&controller_proto::GetStorageOverviewResponse>,
) -> Vec<VolumeRowDto> {
    let mut rows = volumes_from_proto(vols);
    let Some(storage) = storage else {
        return rows;
    };
    for node in &storage.nodes {
        if !node.lvm_inventory_ok {
            continue;
        }
        for lv in &node.lvm_logical_volumes {
            let name = lv.name.trim();
            if name.is_empty() {
                continue;
            }
            let pool = lv.vg_name.trim();
            let already = rows.iter().any(|row| {
                row.backend == "LVM"
                    && row.node_id == node.node_id
                    && row.pool == pool
                    && row.name == name
            });
            if already {
                continue;
            }
            rows.push(VolumeRowDto {
                name: name.to_string(),
                role: String::new(),
                attach_state: String::new(),
                size: format::bytes_human(lv.size_bytes),
                vm: String::new(),
                encrypted: false,
                backend: "LVM".to_string(),
                pool: pool.to_string(),
                node_id: node.node_id.clone(),
            });
        }
    }
    sort_volume_rows(&mut rows);
    rows
}

fn sort_volume_rows(rows: &mut [VolumeRowDto]) {
    rows.sort_by(|a, b| {
        volume_backend_rank(&a.backend)
            .cmp(&volume_backend_rank(&b.backend))
            .then(a.name.cmp(&b.name))
            .then(a.node_id.cmp(&b.node_id))
    });
}

fn volume_backend_rank(backend: &str) -> u8 {
    match backend {
        "Ceph" => 0,
        "LVM" => 1,
        "ZFS" => 2,
        _ => 3,
    }
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

pub fn ceph_cluster_detail_from_proto(c: controller_proto::CephCluster) -> CephClusterDetailDto {
    let spec = c.spec.unwrap_or_default();
    let status = c.status.unwrap_or_default();
    let created_at = c
        .created_at
        .as_ref()
        .map(|t| format::format_unix_utc(t.seconds))
        .unwrap_or_else(|| "—".to_string());
    let updated_at = c
        .updated_at
        .as_ref()
        .map(|t| format::format_unix_utc(t.seconds))
        .unwrap_or_else(|| "—".to_string());

    let mut members: Vec<CephMemberDto> = spec
        .nodes
        .iter()
        .map(|n| {
            let mut devices = vec![n.osd_device.clone()];
            for extra in &n.osd_devices {
                if !extra.is_empty() && !devices.iter().any(|have| have == extra) {
                    devices.push(extra.clone());
                }
            }
            CephMemberDto {
                node_id: n.node_id.clone(),
                mon_addr: n.mon_addr.clone(),
                cluster_addr: n.cluster_addr.clone(),
                public_iface: n.public_iface.clone(),
                cluster_iface: n.cluster_iface.clone(),
                osd_devices: join_nonempty(devices),
            }
        })
        .collect();
    members.sort_by(|a, b| a.node_id.cmp(&b.node_id));

    let health = health_from_status_json(&status.ceph_status_json);
    let mut capacity = capacity_from_status_json(&status.ceph_status_json);
    let cluster_slices = vec![
        ("used".to_string(), capacity.used_bytes),
        ("available".to_string(), capacity.avail_bytes),
    ];
    let cluster_pie_svg = ceph_status::pie_svg(&cluster_slices, "Cluster capacity");
    let class_pie_svg = ceph_status::pie_svg(&[], "Device class capacity");

    if capacity.source.is_empty() {
        capacity.source = "none".to_string();
    }

    CephClusterDetailDto {
        name: c.name,
        generation: c.generation,
        fsid: spec.fsid,
        phase: format::service_phase_label(status.phase).to_string(),
        health_message: status.health_message,
        public_network: spec.public_network,
        cluster_network: spec.cluster_network,
        size: spec.size,
        min_size: spec.min_size,
        encrypt_osds: spec.encrypt_osds,
        force_wipe: spec.force_wipe,
        created_at,
        updated_at,
        members,
        health,
        capacity,
        cluster_pie_svg,
        class_pie_svg,
    }
}

/// Prefer live `ceph df` capacity when the controller returned it; keep pgmap fallback otherwise.
pub fn merge_ceph_capacity(
    fallback: crate::dto::CephCapacityDto,
    live: controller_proto::GetCephClusterCapacityResponse,
) -> crate::dto::CephCapacityDto {
    if !live.ok {
        let mut out = fallback;
        if out.source.is_empty() || out.source == "none" {
            out.source = "none".to_string();
        }
        if !live.message.is_empty() {
            out.error = live.message;
        }
        return out;
    }
    let by_class = live
        .by_class
        .into_iter()
        .map(|c| {
            let used_percent = if c.total_bytes > 0 {
                ((c.used_bytes as f64) * 100.0 / (c.total_bytes as f64)).round() as i32
            } else {
                0
            };
            CephDeviceClassDto {
                device_class: c.device_class,
                total_bytes: c.total_bytes,
                used_bytes: c.used_bytes,
                avail_bytes: c.avail_bytes,
                total_human: format::bytes_human(c.total_bytes),
                used_human: format::bytes_human(c.used_bytes),
                avail_human: format::bytes_human(c.avail_bytes),
                used_percent,
            }
        })
        .collect::<Vec<_>>();
    let pools = live
        .pools
        .into_iter()
        .map(|p| CephPoolUsageDto {
            name: p.name,
            stored_bytes: p.stored_bytes,
            stored_human: format::bytes_human(p.stored_bytes),
            objects: p.objects,
            percent_used_bp: p.percent_used_bp,
            max_avail_bytes: p.max_avail_bytes,
            max_avail_human: format::bytes_human(p.max_avail_bytes),
        })
        .collect();
    let mut out = capacity_dto(
        live.total_bytes,
        live.used_bytes,
        live.avail_bytes,
        by_class,
        pools,
        "df",
        "",
    );
    out.queried_node = live.queried_node;
    out
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

const POSTGRES_ENGINE: &str = "PostgreSQL";
const POSTGRES_DEFAULT_PACKAGE: &str = "pkgs.postgresql";
const POSTGRES_DEFAULT_PORT: i32 = 5432;
const POSTGRES_ACCESS: &str = "Local Unix socket on the node. TCP is not opened.";

fn postgres_package(package: &str) -> String {
    let package = package.trim();
    if package.is_empty() {
        POSTGRES_DEFAULT_PACKAGE.to_string()
    } else {
        package.to_string()
    }
}

fn postgres_port(port: i32) -> i32 {
    if port <= 0 {
        POSTGRES_DEFAULT_PORT
    } else {
        port
    }
}

pub fn dbms_from_postgresqls(rows: Vec<controller_proto::PostgresqlInfo>) -> Vec<DbmsRowDto> {
    let mut rows: Vec<DbmsRowDto> = rows
        .into_iter()
        .map(|info| DbmsRowDto {
            name: info.name,
            engine: POSTGRES_ENGINE.to_string(),
            database: info.database,
            package: postgres_package(&info.package),
            port: postgres_port(info.port),
            node_id: info.node_id,
        })
        .collect();
    rows.sort_by(|a, b| a.name.cmp(&b.name));
    rows
}

pub fn dbms_detail_from_postgresql(info: controller_proto::PostgresqlInfo) -> DbmsDetailDto {
    DbmsDetailDto {
        name: info.name,
        engine: POSTGRES_ENGINE.to_string(),
        database: info.database,
        package: postgres_package(&info.package),
        port: postgres_port(info.port),
        node_id: info.node_id,
        access: POSTGRES_ACCESS.to_string(),
    }
}

fn kubernetes_node_dto(node: controller_proto::KubernetesNodeSpec) -> KubernetesNodeDto {
    KubernetesNodeDto {
        node_id: node.node_id,
        role: node.role,
        advertise_address: node.advertise_address,
        iface: node.iface,
        initial_cluster: node.initial_cluster,
        join_existing: node.join_existing,
    }
}

fn kubernetes_nodes(nodes: Vec<controller_proto::KubernetesNodeSpec>) -> Vec<KubernetesNodeDto> {
    let mut rows: Vec<KubernetesNodeDto> = nodes.into_iter().map(kubernetes_node_dto).collect();
    rows.sort_by(|a, b| a.node_id.cmp(&b.node_id));
    rows
}

pub fn kubernetes_clusters_from_proto(
    clusters: Vec<controller_proto::KubernetesCluster>,
) -> Vec<KubernetesClusterRowDto> {
    let mut rows: Vec<KubernetesClusterRowDto> = clusters
        .into_iter()
        .map(|cluster| {
            let spec = cluster.spec.unwrap_or_default();
            let status = cluster.status.unwrap_or_default();
            KubernetesClusterRowDto {
                name: cluster.name,
                phase: format::kubernetes_phase_label(&status.phase),
                message: status.message,
                cni: spec.cni,
                api_server_endpoint: spec.api_server_endpoint,
                control_planes: spec.control_plane_nodes.len() as i32,
                workers: spec.worker_nodes.len() as i32,
                ready_control_planes: status.ready_control_planes,
                ready_workers: status.ready_workers,
            }
        })
        .collect();
    rows.sort_by(|a, b| a.name.cmp(&b.name));
    rows
}

pub fn kubernetes_detail_from_proto(
    cluster: controller_proto::KubernetesCluster,
    health: Result<controller_proto::GetKubernetesClusterHealthResponse, String>,
) -> KubernetesClusterDetailDto {
    let spec = cluster.spec.unwrap_or_default();
    let status = cluster.status.unwrap_or_default();
    let cilium_configured = spec.cilium_config.is_some();
    let cilium = spec.cilium_config.unwrap_or_default();
    let ha_configured = spec.high_availability.is_some();
    let ha = spec.high_availability.unwrap_or_default();
    let schedule_on_control_plane = spec.schedule_on_control_plane;
    let local = spec.local_provisioner.unwrap_or_default();
    let local_provisioner_configured = local.enabled || !local.volumes.is_empty();
    let local_provisioner_mode = if local_provisioner_configured {
        format::local_volume_mode_label(&local.mode)
    } else {
        String::new()
    };
    let local_provisioner_class = if !local_provisioner_configured {
        String::new()
    } else if local.storage_class_name.trim().is_empty() {
        "local-path".to_string()
    } else {
        local.storage_class_name.clone()
    };
    let cluster_name = cluster.name.clone();
    let local_volumes = local
        .volumes
        .into_iter()
        .map(|volume| kubernetes_host_volume(volume, &cluster_name))
        .collect();
    let (overall_health, health_fields, health_error, node_health) = match health {
        Ok(health) => {
            let nodes = health
                .node_health
                .into_iter()
                .map(|node| KubernetesNodeHealthDto {
                    node_id: node.node_id,
                    kubernetes_status: node.kubernetes_status,
                    kubelet_active: node.kubelet_active,
                    etcd_active: node.etcd_active,
                    api_server_active: node.api_server_active,
                })
                .collect::<Vec<_>>();
            (
                format::kubernetes_phase_label(&health.overall_health),
                (
                    health.ready_control_planes,
                    health.total_control_planes,
                    health.ready_workers,
                    health.total_workers,
                    health.etcd_healthy,
                    health.etcd_member_count,
                    health.api_server_reachable,
                ),
                String::new(),
                nodes,
            )
        }
        Err(error) => (
            "Unknown".to_string(),
            (0, 0, 0, 0, false, 0, false),
            error,
            Vec::new(),
        ),
    };
    KubernetesClusterDetailDto {
        name: cluster.name,
        generation: cluster.generation,
        phase: format::kubernetes_phase_label(&status.phase),
        message: status.message,
        last_updated: timestamp_label(status.last_updated),
        active_node_ids: join_nonempty(status.active_node_ids),
        cluster_cidr: spec.cluster_cidr,
        service_cidr: spec.service_cidr,
        cluster_dns: spec.cluster_dns,
        api_server_endpoint: spec.api_server_endpoint,
        cni: spec.cni,
        cilium_configured,
        cilium_mode: cilium.mode,
        cilium_mtu: cilium.mtu,
        cilium_tunnel: cilium.tunnel,
        cilium_auto_direct: cilium.auto_direct_node_routes,
        cilium_lb_pools: join_nonempty(cilium.load_balancer_ip_pools),
        ha_configured,
        ha_enabled: ha.enabled,
        ha_vip: ha.vip_address,
        ha_interface: ha.vip_interface,
        ha_method: ha.method,
        control_plane_nodes: kubernetes_nodes(spec.control_plane_nodes),
        worker_nodes: kubernetes_nodes(spec.worker_nodes),
        overall_health,
        health_ready_control_planes: health_fields.0,
        health_total_control_planes: health_fields.1,
        health_ready_workers: health_fields.2,
        health_total_workers: health_fields.3,
        etcd_healthy: health_fields.4,
        etcd_member_count: health_fields.5,
        api_server_reachable: health_fields.6,
        node_health,
        health_error,
        schedule_on_control_plane,
        local_provisioner_configured,
        local_provisioner_mode,
        local_provisioner_class,
        local_volumes,
    }
}

fn kubernetes_host_volume(
    volume: controller_proto::KubernetesHostVolume,
    cluster_name: &str,
) -> KubernetesHostVolumeDto {
    let name = volume.name.clone();
    let role = if volume.role.trim() == "pool" {
        "Thin pool"
    } else {
        "Filesystem"
    };
    let mount_path = if role == "Thin pool" {
        if volume.volume_group.trim().is_empty() {
            "—".to_string()
        } else {
            volume.volume_group.clone()
        }
    } else if volume.mount_path.trim().is_empty() {
        format!("/mnt/kcore/{cluster_name}/{name}")
    } else {
        volume.mount_path.clone()
    };
    let fs_type = if volume.fs_type.trim().is_empty() {
        "ext4".to_string()
    } else {
        volume.fs_type
    };
    KubernetesHostVolumeDto {
        name,
        node_id: volume.node_id,
        storage_class: format::storage_backend_name(&volume.storage_class),
        size: format::bytes_human(volume.size_bytes),
        fs_type,
        mount_path,
        role: role.to_string(),
        volume_group: volume.volume_group,
    }
}

pub fn vm_detail_from_proto(
    response: controller_proto::GetVmResponse,
    migrate: Result<controller_proto::GetLiveMigrateReceiveStatusResponse, String>,
) -> VmDetailDto {
    let spec = response.spec.unwrap_or_default();
    let status = response.status.unwrap_or_default();
    let (migrate_nodes, migrate_error) = match migrate {
        Ok(status) => (migrate_nodes_from_proto(status), String::new()),
        Err(error) => (Vec::new(), error),
    };
    VmDetailDto {
        id: spec.id,
        name: spec.name,
        state: format::vm_state_label(status.state).to_string(),
        desired: format::desired_state_label(spec.desired_state).to_string(),
        node_id: response.node_id,
        cpu: spec.cpu,
        memory: format::memory_mebibytes(spec.memory_bytes),
        disk_size: format::bytes_human(spec.storage_size_bytes),
        backend: format::storage_backend_name(&spec.storage_backend),
        created: timestamp_label(status.created_at),
        updated: timestamp_label(status.updated_at),
        assigned_ip: response.assigned_ip,
        disks: spec
            .disks
            .into_iter()
            .map(|disk| VmDiskDto {
                name: disk.name,
                path: disk.backend_handle,
                bus: disk.bus,
                device: disk.device,
            })
            .collect(),
        nics: spec
            .nics
            .into_iter()
            .map(|nic| VmNicDto {
                network: nic.network,
                model: nic.model,
                mac: nic.mac_address,
            })
            .collect(),
        gpus: spec.gpus,
        pci: spec.pci_devices,
        migrate: migrate_nodes,
        migrate_error,
    }
}

fn migrate_nodes_from_proto(
    status: controller_proto::GetLiveMigrateReceiveStatusResponse,
) -> Vec<MigrateNodeDto> {
    status
        .nodes
        .into_iter()
        .map(|node| {
            let state = node.state.unwrap_or_default();
            MigrateNodeDto {
                node_id: node.node_id,
                address: node.node_address,
                reachable: node.reachable,
                error: node.error,
                has_session: state.has_session,
                owns_vm: node.owns_vm,
                summary: state.summary,
                port: state.port,
                vmm_alive: state.vmm_alive,
            }
        })
        .collect()
}

pub fn volume_detail_from_info(
    volume: controller_proto::VolumeInfo,
    registered: bool,
) -> VolumeDetailDto {
    let vm = volume_vm_label(&volume);
    let slot = if registered {
        volume.slot.to_string()
    } else {
        "—".to_string()
    };
    VolumeDetailDto {
        id: volume.id,
        name: volume.name,
        role: volume.role,
        attach_state: volume.attach_state,
        size: format::bytes_human(volume.storage_size_bytes),
        guest_size: format::guest_size_label(volume.guest_visible_bytes),
        vm,
        encrypted: volume.encrypted,
        backend: format::storage_backend_name(&volume.storage_backend),
        pool: volume.pool,
        image: volume.image,
        node_id: volume.node_id,
        serial: volume.serial,
        slot,
        mount_point: volume.mount_point,
        fs_type: volume.fs_type,
        path: volume.backend_handle,
        image_format: volume.image_format,
        registered,
        note: String::new(),
    }
}

/// An LVM logical volume that the node reported but the controller has not registered.
pub fn inventory_volume_detail(
    name: &str,
    node: &str,
    storage: &controller_proto::GetStorageOverviewResponse,
) -> Option<VolumeDetailDto> {
    let name = name.trim();
    let node = node.trim();
    for host in &storage.nodes {
        if !node.is_empty() && host.node_id != node {
            continue;
        }
        if !host.lvm_inventory_ok {
            continue;
        }
        for lv in &host.lvm_logical_volumes {
            if lv.name.trim() != name {
                continue;
            }
            return Some(VolumeDetailDto {
                id: String::new(),
                name: lv.name.clone(),
                role: String::new(),
                attach_state: String::new(),
                size: format::bytes_human(lv.size_bytes),
                guest_size: "Not checked".to_string(),
                vm: String::new(),
                encrypted: false,
                backend: "LVM".to_string(),
                pool: lv.vg_name.clone(),
                image: String::new(),
                node_id: host.node_id.clone(),
                serial: String::new(),
                slot: "—".to_string(),
                mount_point: String::new(),
                fs_type: String::new(),
                path: lv.path.clone(),
                image_format: String::new(),
                registered: false,
                note: "Reported by the node. It is not a registered kcore volume, so attach state and encryption are unknown.".to_string(),
            });
        }
    }
    None
}

pub fn snapshots_from_proto(
    snapshots: Vec<controller_proto::VolumeSnapshotInfo>,
) -> Vec<SnapshotRowDto> {
    let mut rows: Vec<SnapshotRowDto> = snapshots
        .into_iter()
        .map(|snap| SnapshotRowDto {
            id: snap.id,
            name: snap.name,
            volume_id: snap.volume_id,
            volume_name: snap.volume_name,
            size: format::bytes_human(snap.size_bytes),
            protected: snap.protected,
            consistency: snap.consistency,
            created: timestamp_label(snap.created_at),
        })
        .collect();
    rows.sort_by(|a, b| a.name.cmp(&b.name));
    rows
}

pub fn snapshot_policies_from_proto(
    policies: Vec<controller_proto::SnapshotPolicy>,
) -> Vec<SnapshotPolicyDto> {
    let mut rows: Vec<SnapshotPolicyDto> = policies
        .into_iter()
        .map(snapshot_policy_from_proto)
        .collect();
    rows.sort_by(|a, b| a.name.cmp(&b.name));
    rows
}

pub fn snapshot_policy_from_proto(policy: controller_proto::SnapshotPolicy) -> SnapshotPolicyDto {
    let applies_to = if !policy.selector_volume.is_empty() && !policy.selector_vm.is_empty() {
        format!(
            "Volume {} on VM {}",
            policy.selector_volume, policy.selector_vm
        )
    } else if !policy.selector_volume.is_empty() {
        format!("Volume {}", policy.selector_volume)
    } else if !policy.selector_vm.is_empty() {
        format!("VM {}", policy.selector_vm)
    } else {
        "Every volume".to_string()
    };
    SnapshotPolicyDto {
        name: policy.name,
        selector_vm: policy.selector_vm,
        selector_volume: policy.selector_volume,
        applies_to,
        schedule: format::schedule_label(&policy.schedule),
        keep: policy.keep,
        enabled: policy.enabled,
        last_run: policy.last_run_at,
        last_message: policy.last_message,
    }
}

pub fn shared_filesystem_detail_from_proto(
    fs: controller_proto::SharedFilesystem,
) -> SharedFilesystemDetailDto {
    let spec = fs.spec.unwrap_or_default();
    let status = fs.status.unwrap_or_default();
    let quota = if spec.quota_bytes <= 0 {
        "No quota".to_string()
    } else {
        format::bytes_human(spec.quota_bytes)
    };
    let fs_name = if spec.fs_name.trim().is_empty() {
        fs.name.clone()
    } else {
        spec.fs_name
    };
    let clients = spec
        .clients
        .into_iter()
        .map(|client| {
            if client.paths.is_empty() {
                client.name
            } else {
                format!("{} ({})", client.name, client.paths.join(", "))
            }
        })
        .collect();
    SharedFilesystemDetailDto {
        name: fs.name,
        ceph_cluster: spec.ceph_cluster,
        fs_name,
        phase: format::service_phase_label(status.phase).to_string(),
        health_message: status.health_message,
        quota,
        generation: fs.generation,
        clients,
    }
}

pub fn object_store_detail_from_proto(
    store: controller_proto::ObjectStore,
) -> ObjectStoreDetailDto {
    let spec = store.spec.unwrap_or_default();
    let status = store.status.unwrap_or_default();
    let rgw_id = if spec.rgw_id.trim().is_empty() {
        store.name.clone()
    } else {
        spec.rgw_id
    };
    ObjectStoreDetailDto {
        name: store.name,
        ceph_cluster: spec.ceph_cluster,
        rgw_id,
        phase: format::service_phase_label(status.phase).to_string(),
        health_message: status.health_message,
        port: spec.port,
        tls: spec.tls,
        generation: store.generation,
        members: spec.members,
    }
}

pub fn operation_detail_from_proto(op: controller_proto::VmOperation) -> OperationDetailDto {
    let updated_at = timestamp_label(op.updated_at);
    let detail_lines = detail_lines(&op.detail_json);
    let mut row = vm_operations_from_proto(vec![op]);
    OperationDetailDto {
        row: row.pop().unwrap_or_else(|| VmOperationRowDto {
            id: String::new(),
            kind: String::new(),
            phase: String::new(),
            vm: String::new(),
            cancel_requested: false,
            send_succeeded: false,
            source_node: String::new(),
            target_node: String::new(),
            started_at: String::new(),
            finished_at: String::new(),
            detail: String::new(),
        }),
        updated_at,
        detail_lines,
    }
}

pub fn detail_lines(raw: &str) -> Vec<(String, String)> {
    let trimmed = raw.trim();
    if trimmed.is_empty() || trimmed == "{}" || trimmed == "null" {
        return Vec::new();
    }
    if let Ok(serde_json::Value::Object(map)) = serde_json::from_str::<serde_json::Value>(trimmed) {
        let mut lines: Vec<(String, String)> = map
            .into_iter()
            .map(|(key, value)| (key, json_scalar(value)))
            .collect();
        lines.sort_by(|a, b| a.0.cmp(&b.0));
        return lines;
    }
    vec![("Detail".to_string(), trimmed.to_string())]
}

fn json_scalar(value: serde_json::Value) -> String {
    match value {
        serde_json::Value::String(text) => text,
        serde_json::Value::Null => "—".to_string(),
        other => other.to_string(),
    }
}

pub fn node_from_proto(node: controller_proto::NodeInfo) -> NodeRowDto {
    let (cpu, memory) = match node.capacity {
        Some(cap) => (
            cap.cpu_cores.to_string(),
            format::bytes_human(cap.memory_bytes),
        ),
        None => ("—".to_string(), "—".to_string()),
    };
    let (cpu_used, memory_used) = match node.usage {
        Some(usage) => (
            usage.cpu_cores_used.to_string(),
            format::bytes_human(usage.memory_bytes_used),
        ),
        None => ("—".to_string(), "—".to_string()),
    };
    let labels = if node.labels.is_empty() {
        "—".to_string()
    } else {
        node.labels.join(", ")
    };
    NodeRowDto {
        id: node.node_id,
        hostname: node.hostname,
        address: node.address,
        status: node.status,
        approval: node.approval_status,
        dc: node.dc_id,
        cpu,
        memory,
        cpu_used,
        memory_used,
        backend: format::storage_backend_label(node.storage_backend).to_string(),
        luks: if node.luks_method.is_empty() {
            "Unknown".to_string()
        } else {
            node.luks_method
        },
        cert: format::cert_expiry_label(node.cert_expiry_days),
        heartbeat: timestamp_label(node.last_heartbeat),
        labels,
        vxlan: if node.disable_vxlan {
            "Off".to_string()
        } else {
            "On".to_string()
        },
    }
}

pub fn nodes_from_proto(nodes: Vec<controller_proto::NodeInfo>) -> Vec<NodeRowDto> {
    let mut rows: Vec<NodeRowDto> = nodes.into_iter().map(node_from_proto).collect();
    rows.sort_by(|a, b| a.hostname.cmp(&b.hostname).then(a.id.cmp(&b.id)));
    rows
}

pub fn node_detail_from_proto(
    node: controller_proto::NodeInfo,
    devices: Result<Vec<controller_proto::GpuInfo>, String>,
) -> NodeDetailDto {
    let (devices, devices_error) = match devices {
        Ok(rows) => (devices_from_proto(rows), String::new()),
        Err(error) => (Vec::new(), error),
    };
    NodeDetailDto {
        node: node_from_proto(node),
        devices,
        devices_error,
    }
}

fn devices_from_proto(devices: Vec<controller_proto::GpuInfo>) -> Vec<DeviceDto> {
    let mut rows: Vec<DeviceDto> = devices
        .into_iter()
        .map(|device| {
            let availability = if !device.assigned_vm.is_empty() {
                "In use".to_string()
            } else if device.assignable {
                "Free".to_string()
            } else {
                "Not assignable".to_string()
            };
            let iommu = if device.iommu_group < 0 {
                "—".to_string()
            } else {
                device.iommu_group.to_string()
            };
            DeviceDto {
                name: device.name,
                kind: format::device_kind_label(&device.kind).to_string(),
                family: device.family,
                model: device.model,
                address: device.address,
                availability,
                blocked_reason: device.blocked_reason,
                assigned_vm: device.assigned_vm,
                role: format::device_role_label(&device.role),
                driver: device.driver,
                iommu,
            }
        })
        .collect();
    rows.sort_by(|a, b| a.kind.cmp(&b.kind).then(a.name.cmp(&b.name)));
    rows
}

pub fn containers_from_proto(
    containers: Vec<controller_proto::ContainerInfo>,
) -> Vec<ContainerRowDto> {
    let mut rows: Vec<ContainerRowDto> = containers
        .into_iter()
        .map(|container| ContainerRowDto {
            id: container.id,
            name: container.name,
            image: container.image,
            state: format::container_state_label(container.state).to_string(),
            status: container.status,
            node_id: container.node_id,
        })
        .collect();
    rows.sort_by(|a, b| a.name.cmp(&b.name));
    rows
}

pub fn container_detail_from_proto(
    response: controller_proto::GetWorkloadResponse,
) -> ContainerDetailDto {
    let spec = response.container_spec.unwrap_or_default();
    let info = response.container_info.unwrap_or_default();
    let mut env: Vec<String> = spec
        .env
        .into_iter()
        .map(|(key, value)| format!("{key}={value}"))
        .collect();
    env.sort();
    ContainerDetailDto {
        id: info.id,
        name: if info.name.is_empty() {
            spec.name
        } else {
            info.name
        },
        image: if info.image.is_empty() {
            spec.image
        } else {
            info.image
        },
        state: format::container_state_label(info.state).to_string(),
        status: info.status,
        node_id: response.node_id,
        desired: format::desired_state_label(spec.desired_state).to_string(),
        network: spec.network,
        command: if spec.command.is_empty() {
            "—".to_string()
        } else {
            spec.command.join(" ")
        },
        env,
        ports: if spec.ports.is_empty() {
            "—".to_string()
        } else {
            spec.ports.join(", ")
        },
        backend: format::storage_backend_name(&spec.storage_backend),
        disk_size: format::bytes_human(spec.storage_size_bytes),
        mount: spec.mount_target,
        created: timestamp_label(info.created_at),
        assigned_ip: response.assigned_ip,
    }
}

pub fn disk_layouts_from_proto(
    layouts: Vec<controller_proto::DiskLayoutSummary>,
) -> Vec<DiskLayoutRowDto> {
    let mut rows: Vec<DiskLayoutRowDto> = layouts
        .into_iter()
        .map(|summary| disk_layout_row(summary.disk_layout.unwrap_or_default(), summary.status))
        .collect();
    rows.sort_by(|a, b| a.name.cmp(&b.name));
    rows
}

fn disk_layout_row(
    layout: controller_proto::DiskLayout,
    status: Option<controller_proto::DiskLayoutStatus>,
) -> DiskLayoutRowDto {
    let status = status.unwrap_or_default();
    DiskLayoutRowDto {
        name: layout.name,
        node_id: layout.node_id,
        phase: format::disk_layout_phase_label(status.phase).to_string(),
        generation: layout.generation,
        observed_generation: status.observed_generation,
        evacuate: layout.evacuate,
        message: status.message,
    }
}

pub fn disk_layout_detail_from_proto(
    response: controller_proto::GetDiskLayoutResponse,
) -> DiskLayoutDetailDto {
    let layout = response.disk_layout.unwrap_or_default();
    let status = response.status.unwrap_or_default();
    let updated = timestamp_label(layout.updated_at);
    DiskLayoutDetailDto {
        refusal_reason: status.refusal_reason.clone(),
        layout_nix: layout.layout_nix.clone(),
        updated,
        row: disk_layout_row(layout, Some(status)),
    }
}

pub fn storage_class_blurb(name: &str) -> &'static str {
    match name {
        "filesystem" => "VM disks are files on the node.",
        "lvm" => "VM disks are LVM logical volumes.",
        "zfs" => "VM disks are ZFS volumes.",
        "ceph" => "VM disks are Ceph RBD images.",
        _ => "This node has not declared a storage type.",
    }
}

pub fn storage_classes_from_nodes(nodes: &[NodeRowDto]) -> Vec<StorageClassDto> {
    const ORDER: [&str; 5] = ["filesystem", "lvm", "zfs", "ceph", "unspecified"];
    ORDER
        .into_iter()
        .map(|name| {
            let count = nodes
                .iter()
                .filter(|node| node.backend.eq_ignore_ascii_case(class_backend_label(name)))
                .count() as i32;
            StorageClassDto {
                name: name.to_string(),
                blurb: storage_class_blurb(name).to_string(),
                nodes: count,
            }
        })
        .collect()
}

fn class_backend_label(name: &str) -> &'static str {
    match name {
        "filesystem" => "Filesystem",
        "lvm" => "LVM",
        "zfs" => "ZFS",
        "ceph" => "Ceph",
        _ => "Unspecified",
    }
}

pub fn storage_class_detail(name: &str, nodes: Vec<NodeRowDto>) -> StorageClassDetailDto {
    let key = name.trim().to_ascii_lowercase();
    let label = class_backend_label(&key);
    let blurb = storage_class_blurb(&key).to_string();
    let nodes = nodes
        .into_iter()
        .filter(|node| node.backend.eq_ignore_ascii_case(label))
        .collect();
    let name = if matches!(
        key.as_str(),
        "filesystem" | "lvm" | "zfs" | "ceph" | "unspecified"
    ) {
        key
    } else {
        name.trim().to_string()
    };
    StorageClassDetailDto { name, blurb, nodes }
}

pub fn certificates_from_proto(
    certs: Vec<controller_proto::CertificateInfo>,
) -> Vec<CertificateRowDto> {
    let mut rows: Vec<CertificateRowDto> = certs
        .into_iter()
        .map(|cert| {
            let kind = match cert.identity_kind.as_str() {
                "node" => "Node",
                "operator" => "Operator",
                "controller" => "Controller",
                "sub-ca" => "Sub-CA",
                other => other,
            };
            CertificateRowDto {
                serial: cert.serial_hex,
                subject: cert.subject_cn,
                kind: kind.to_string(),
                node_id: cert.node_id,
                issuer: cert.issuer_cn,
                status: format::certificate_status_label(cert.status).to_string(),
                expires: format::cert_expiry_label(cert.days_until_expiry),
                reason: if cert.status == 3 {
                    format::revocation_reason_label(cert.revocation_reason).to_string()
                } else {
                    "—".to_string()
                },
            }
        })
        .collect();
    rows.sort_by(|a, b| a.expires.cmp(&b.expires).then(a.subject.cmp(&b.subject)));
    rows
}

pub fn pki_status_from_proto(status: controller_proto::GetPkiStatusResponse) -> PkiStatusDto {
    PkiStatusDto {
        active: status.active_count,
        rotated: status.rotated_count,
        revoked: status.revoked_count,
        expired: status.expired_count,
        expiring_soon: status.expiring_soon_count,
        warn_before_days: status.warn_before_days,
        renew_before_days: status.renew_before_days,
        rotation_enabled: status.rotation_enabled,
        crl_number: status.crl_number,
        crl_available: status.crl_available,
        sub_ca_available: status.sub_ca_available,
        revocation_mode: status.revocation_fail_mode,
        pki_url: status.pki_http_base_url,
    }
}

pub fn crl_summary_from_proto(crl: controller_proto::GetCrlResponse) -> CrlSummaryDto {
    CrlSummaryDto {
        available: crl.success,
        number: crl.crl_number,
        revoked: crl.revoked_count,
        this_update: timestamp_label(crl.this_update),
        next_update: timestamp_label(crl.next_update),
        message: crl.message,
    }
}

pub fn cluster_health_from_proto(
    health: controller_proto::GetClusterHealthResponse,
) -> ClusterHealthDto {
    let headline = match health.status.trim().to_ascii_lowercase().as_str() {
        "healthy" => "Cluster is healthy",
        "degraded" => "Cluster needs attention",
        "unhealthy" => "Cluster is unhealthy",
        other => {
            if other.is_empty() {
                "Cluster status is unknown"
            } else {
                return ClusterHealthDto {
                    headline: format::kubernetes_phase_label(other),
                    status: health.status,
                    ready_nodes: health.ready_nodes,
                    not_ready_nodes: health.not_ready_nodes,
                    unschedulable_nodes: health.unschedulable_nodes,
                    pending_nodes: health.pending_nodes,
                    vm_count: health.vm_count,
                    vms_on_not_ready_nodes: health.vms_on_not_ready_nodes,
                    replication_conflicts: health.replication_conflicts,
                    certificates_expiring_soon: health.certificates_expiring_soon,
                    conditions: health.conditions,
                };
            }
        }
    };
    ClusterHealthDto {
        status: health.status,
        headline: headline.to_string(),
        ready_nodes: health.ready_nodes,
        not_ready_nodes: health.not_ready_nodes,
        unschedulable_nodes: health.unschedulable_nodes,
        pending_nodes: health.pending_nodes,
        vm_count: health.vm_count,
        vms_on_not_ready_nodes: health.vms_on_not_ready_nodes,
        replication_conflicts: health.replication_conflicts,
        certificates_expiring_soon: health.certificates_expiring_soon,
        conditions: health.conditions,
    }
}

pub fn ssh_keys_from_proto(keys: Vec<controller_proto::SshKeyInfo>) -> Vec<SshKeyRowDto> {
    let mut rows: Vec<SshKeyRowDto> = keys
        .into_iter()
        .map(|key| SshKeyRowDto {
            name: key.name,
            created: timestamp_label(key.created_at),
        })
        .collect();
    rows.sort_by(|a, b| a.name.cmp(&b.name));
    rows
}

pub fn ssh_key_detail_from_proto(key: controller_proto::SshKeyInfo) -> SshKeyDetailDto {
    SshKeyDetailDto {
        name: key.name,
        created: timestamp_label(key.created_at),
        public_key: key.public_key,
    }
}

pub fn sbom_summary_from_proto(
    kind: &str,
    response: Result<controller_proto::ExportSbomResponse, String>,
) -> SbomSummaryDto {
    let title = match kind {
        "iso-closure" => "Install image",
        _ => "Crate list",
    };
    match response {
        Ok(doc) => {
            let source = match doc.source.as_str() {
                "file" => "Installed file".to_string(),
                "embedded" => "Built into the controller".to_string(),
                "" => "—".to_string(),
                other => other.to_string(),
            };
            SbomSummaryDto {
                kind: kind.to_string(),
                title: title.to_string(),
                format: doc.format,
                spec_version: doc.spec_version,
                filename: doc.filename,
                sha256: doc.sha256,
                source,
                generator: doc.generator,
                signed: doc.signature_present,
                signing_scheme: doc.signing_scheme,
                error: String::new(),
            }
        }
        Err(error) => SbomSummaryDto {
            kind: kind.to_string(),
            title: title.to_string(),
            format: String::new(),
            spec_version: String::new(),
            filename: String::new(),
            sha256: String::new(),
            source: String::new(),
            generator: String::new(),
            signed: false,
            signing_scheme: String::new(),
            error,
        },
    }
}

pub fn crypto_config_from_proto(cfg: controller_proto::GetCryptoConfigResponse) -> CryptoConfigDto {
    let rate_limit = if cfg.rate_limit_enabled {
        format!(
            "{} requests/s, burst {}",
            cfg.rate_limit_requests_per_second, cfg.rate_limit_burst
        )
    } else {
        "Off".to_string()
    };
    CryptoConfigDto {
        fips_certificate: cfg.fips_certificate,
        rate_limit,
        revocation_mode: cfg.revocation_fail_mode,
        signing_scheme: cfg.signing_scheme,
    }
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
                ..Default::default()
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
                labels: std::collections::HashMap::new(),
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
                labels: std::collections::HashMap::new(),
            },
        ];
        let p = vms_page_from_proto(vms, 1, 10);
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
    fn host_lvm_inventory_joins_registered_ceph_and_zfs() {
        let vols = vec![
            controller_proto::VolumeInfo {
                name: "rbd-root".into(),
                storage_backend: "ceph".into(),
                pool: "kcore-vms".into(),
                node_id: "node-103".into(),
                ..Default::default()
            },
            controller_proto::VolumeInfo {
                name: "tank-data".into(),
                storage_backend: "zfs".into(),
                pool: "tank0".into(),
                node_id: "node-119".into(),
                ..Default::default()
            },
            controller_proto::VolumeInfo {
                name: "kcore-kdata".into(),
                storage_backend: "lvm".into(),
                pool: "vg_kcore".into(),
                node_id: "node-107".into(),
                ..Default::default()
            },
        ];
        let storage = controller_proto::GetStorageOverviewResponse {
            nodes: vec![controller_proto::NodeStorageOverview {
                node_id: "node-107".into(),
                lvm_inventory_ok: true,
                lvm_logical_volumes: vec![
                    controller_proto::StorageLvmLogicalVolumeDetail {
                        name: "kcore-kdata".into(),
                        vg_name: "vg_kcore".into(),
                        size_bytes: 50 * 1024 * 1024 * 1024,
                        path: "/dev/vg_kcore/kcore-kdata".into(),
                        ..Default::default()
                    },
                    controller_proto::StorageLvmLogicalVolumeDetail {
                        name: "kcore-win-01".into(),
                        vg_name: "vg_kcore".into(),
                        size_bytes: 40 * 1024 * 1024 * 1024,
                        path: "/dev/vg_kcore/kcore-win-01".into(),
                        ..Default::default()
                    },
                ],
                ..Default::default()
            }],
            ..Default::default()
        };
        let rows = volumes_with_host_inventory(vols, Some(&storage));
        let names: Vec<_> = rows.iter().map(|row| row.name.as_str()).collect();
        assert_eq!(
            names,
            vec!["rbd-root", "kcore-kdata", "kcore-win-01", "tank-data"]
        );
        assert_eq!(rows[0].backend, "Ceph");
        assert_eq!(rows[1].backend, "LVM");
        assert_eq!(rows[2].backend, "LVM");
        assert_eq!(rows[2].pool, "vg_kcore");
        assert_eq!(rows[2].node_id, "node-107");
        assert_eq!(rows[3].backend, "ZFS");
    }

    #[test]
    fn ceph_cluster_detail_maps_spec_and_status_json() {
        let cluster = controller_proto::CephCluster {
            name: "lab-san".into(),
            generation: 3,
            spec: Some(controller_proto::CephClusterSpec {
                fsid: "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee".into(),
                public_network: "192.168.40.0/24".into(),
                cluster_network: "192.168.47.0/24".into(),
                size: 3,
                min_size: 2,
                encrypt_osds: true,
                nodes: vec![controller_proto::CephClusterNodeSpec {
                    node_id: "node-103".into(),
                    mon_addr: "192.168.40.103:6789".into(),
                    cluster_addr: "192.168.47.2".into(),
                    public_iface: "enp6s0".into(),
                    cluster_iface: "enp6s0d1".into(),
                    osd_device: "/dev/sda".into(),
                    osd_devices: vec!["/dev/sdb".into()],
                }],
                ..Default::default()
            }),
            status: Some(controller_proto::CephClusterStatus {
                phase: controller_proto::CephClusterPhase::Healthy as i32,
                health_message: "HEALTH_OK".into(),
                ceph_status_json: r#"{
                  "health":{"status":"HEALTH_OK","checks":{}},
                  "quorum_names":["node-103"],
                  "monmap":{"num_mons":1},
                  "osdmap":{"num_osds":2,"num_up_osds":2,"num_in_osds":2},
                  "pgmap":{
                    "num_pgs":32,
                    "bytes_total":1000,
                    "bytes_used":250,
                    "bytes_avail":750,
                    "pgs_by_state":[{"state_name":"active+clean","count":32}]
                  }
                }"#
                .into(),
                ..Default::default()
            }),
            ..Default::default()
        };
        let detail = ceph_cluster_detail_from_proto(cluster);
        assert_eq!(detail.name, "lab-san");
        assert_eq!(detail.size, 3);
        assert_eq!(detail.min_size, 2);
        assert_eq!(detail.members.len(), 1);
        assert!(detail.members[0].osd_devices.contains("/dev/sda"));
        assert!(detail.members[0].osd_devices.contains("/dev/sdb"));
        assert_eq!(detail.health.osd_up, 2);
        assert_eq!(detail.capacity.used_percent, 25);
        assert_eq!(detail.capacity.source, "pgmap");
        assert!(detail.cluster_pie_svg.contains("<path"));
    }

    #[test]
    fn merge_ceph_capacity_prefers_live_df() {
        let fallback = capacity_from_status_json("");
        let live = controller_proto::GetCephClusterCapacityResponse {
            ok: true,
            queried_node: "node-103".into(),
            total_bytes: 900,
            used_bytes: 300,
            avail_bytes: 600,
            by_class: vec![controller_proto::CephDeviceClassCapacity {
                device_class: "ssd".into(),
                total_bytes: 900,
                used_bytes: 300,
                avail_bytes: 600,
            }],
            pools: vec![controller_proto::CephPoolCapacity {
                name: "kcore-vms".into(),
                stored_bytes: 10,
                objects: 1,
                percent_used_bp: 100,
                max_avail_bytes: 200,
            }],
            ..Default::default()
        };
        let merged = merge_ceph_capacity(fallback, live);
        assert_eq!(merged.source, "df");
        assert_eq!(merged.queried_node, "node-103");
        assert_eq!(merged.by_class.len(), 1);
        assert_eq!(merged.by_class[0].device_class, "ssd");
        assert_eq!(merged.pools[0].name, "kcore-vms");
        assert_eq!(merged.used_percent, 33);
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

    #[test]
    fn postgresql_maps_to_dbms_with_defaults() {
        let rows = dbms_from_postgresqls(vec![
            controller_proto::PostgresqlInfo {
                name: "zeta".into(),
                database: "zeta".into(),
                package: String::new(),
                port: 0,
                node_id: "node-105".into(),
            },
            controller_proto::PostgresqlInfo {
                name: "app-db".into(),
                database: "app".into(),
                package: "postgresql_16".into(),
                port: 5432,
                node_id: "node-107".into(),
            },
        ]);
        assert_eq!(rows[0].name, "app-db");
        assert_eq!(rows[0].engine, "PostgreSQL");
        assert_eq!(rows[0].package, "postgresql_16");
        assert_eq!(rows[1].package, "pkgs.postgresql");
        assert_eq!(rows[1].port, 5432);
        let detail = dbms_detail_from_postgresql(controller_proto::PostgresqlInfo {
            name: "app-db".into(),
            database: "app".into(),
            package: "postgresql_16".into(),
            port: 5432,
            node_id: "node-107".into(),
        });
        assert_eq!(
            detail.access,
            "Local Unix socket on the node. TCP is not opened."
        );
    }

    #[test]
    fn kubernetes_cluster_maps_list_and_detail() {
        let cluster = sample_k8s_cluster();
        let rows = kubernetes_clusters_from_proto(vec![cluster.clone()]);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].name, "lab");
        assert_eq!(rows[0].phase, "Ready");
        assert_eq!(rows[0].control_planes, 1);
        assert_eq!(rows[0].workers, 1);
        assert_eq!(rows[0].cni, "cilium");

        let health = controller_proto::GetKubernetesClusterHealthResponse {
            overall_health: "healthy".into(),
            ready_control_planes: 1,
            total_control_planes: 1,
            ready_workers: 1,
            total_workers: 1,
            etcd_healthy: true,
            etcd_member_count: 1,
            api_server_reachable: true,
            node_health: vec![controller_proto::NodeHealth {
                node_id: "node-107".into(),
                kubernetes_status: "Ready".into(),
                kubelet_active: true,
                etcd_active: true,
                api_server_active: true,
            }],
        };
        let detail = kubernetes_detail_from_proto(cluster, Ok(health));
        assert_eq!(detail.overall_health, "Healthy");
        assert_eq!(detail.api_server_endpoint, "192.168.40.220:6443");
        assert!(detail.cilium_configured);
        assert_eq!(detail.cilium_mode, "native-routing");
        assert!(detail.ha_enabled);
        assert_eq!(detail.control_plane_nodes[0].node_id, "node-107");
        assert_eq!(detail.worker_nodes[0].node_id, "node-119");
        assert!(detail.health_error.is_empty());
        assert_eq!(detail.node_health.len(), 1);
        assert_eq!(detail.last_updated, "2023-11-14 22:13:20Z");

        let failed = kubernetes_detail_from_proto(sample_k8s_cluster(), Err("health down".into()));
        assert_eq!(failed.overall_health, "Unknown");
        assert_eq!(failed.health_error, "health down");
        assert!(failed.node_health.is_empty());
    }

    fn sample_k8s_cluster() -> controller_proto::KubernetesCluster {
        controller_proto::KubernetesCluster {
            name: "lab".into(),
            generation: 4,
            spec: Some(controller_proto::KubernetesClusterSpec {
                cluster_cidr: "10.244.0.0/16".into(),
                service_cidr: "10.96.0.0/12".into(),
                cluster_dns: "10.96.0.10".into(),
                api_server_endpoint: "192.168.40.220:6443".into(),
                cni: "cilium".into(),
                cilium_config: Some(controller_proto::CiliumConfig {
                    mode: "native-routing".into(),
                    mtu: 1500,
                    tunnel: "disabled".into(),
                    auto_direct_node_routes: true,
                    load_balancer_ip_pools: vec!["192.168.40.220/29".into()],
                }),
                control_plane_nodes: vec![controller_proto::KubernetesNodeSpec {
                    node_id: "node-107".into(),
                    role: "control-plane".into(),
                    advertise_address: "192.168.40.107".into(),
                    iface: "enp6s0".into(),
                    initial_cluster: true,
                    join_existing: false,
                }],
                worker_nodes: vec![controller_proto::KubernetesNodeSpec {
                    node_id: "node-119".into(),
                    role: "worker".into(),
                    advertise_address: "192.168.40.119".into(),
                    iface: "enp6s0".into(),
                    initial_cluster: false,
                    join_existing: false,
                }],
                high_availability: Some(controller_proto::HighAvailabilityConfig {
                    enabled: true,
                    vip_address: "192.168.40.220".into(),
                    vip_interface: "enp6s0".into(),
                    method: "keepalived".into(),
                }),
                local_provisioner: None,
                schedule_on_control_plane: false,
            }),
            status: Some(controller_proto::KubernetesClusterStatus {
                phase: "ready".into(),
                message: "all nodes joined".into(),
                ready_control_planes: 1,
                ready_workers: 1,
                last_updated: Some(prost_types::Timestamp {
                    seconds: 1_700_000_000,
                    nanos: 0,
                }),
                active_node_ids: vec!["node-107".into(), "node-119".into()],
            }),
        }
    }

    #[test]
    fn kubernetes_detail_explains_scheduling_and_local_volumes() {
        let mut cluster = sample_k8s_cluster();
        let spec = cluster.spec.as_mut().expect("spec");
        spec.schedule_on_control_plane = true;
        spec.local_provisioner = Some(controller_proto::KubernetesLocalProvisioner {
            enabled: true,
            storage_class_name: String::new(),
            mode: "lvm".into(),
            volumes: vec![controller_proto::KubernetesHostVolume {
                name: "kdata".into(),
                node_id: "node-107".into(),
                storage_class: "lvm".into(),
                size_bytes: 50 * 1024 * 1024 * 1024,
                fs_type: String::new(),
                mount_path: String::new(),
                role: "pool".into(),
                volume_group: "vg_kcore".into(),
            }],
        });
        let detail = kubernetes_detail_from_proto(cluster, Err("health skipped".into()));
        assert!(detail.schedule_on_control_plane);
        assert_eq!(detail.local_provisioner_mode, "LVM thin pool");
        assert_eq!(detail.local_provisioner_class, "local-path");
        assert_eq!(detail.local_volumes[0].role, "Thin pool");
        assert_eq!(detail.local_volumes[0].mount_path, "vg_kcore");
        assert_eq!(detail.local_volumes[0].fs_type, "ext4");
    }

    #[test]
    fn snapshot_policy_schedule_is_plain_language() {
        let policy = snapshot_policy_from_proto(controller_proto::SnapshotPolicy {
            name: "daily".into(),
            selector_volume: "pg".into(),
            schedule: "@daily".into(),
            keep: 7,
            enabled: true,
            ..Default::default()
        });
        assert_eq!(policy.schedule, "Every day");
        assert_eq!(policy.applies_to, "Volume pg");
    }

    #[test]
    fn cluster_health_headline_uses_plain_words() {
        let health = cluster_health_from_proto(controller_proto::GetClusterHealthResponse {
            status: "degraded".into(),
            ready_nodes: 2,
            not_ready_nodes: 1,
            conditions: vec!["node-103 is not ready".into()],
            ..Default::default()
        });
        assert_eq!(health.headline, "Cluster needs attention");
        assert_eq!(health.conditions.len(), 1);
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
