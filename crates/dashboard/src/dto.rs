//! Serializable view models for server functions and tests.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AccessControlEntryDto {
    pub rpc_method: String,
    pub allowed_identities: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct NodeSummaryDto {
    pub node_id: String,
    pub hostname: String,
    pub address: String,
    pub dc_id: String,
    pub approval_status: String,
    pub cert_expiry_days: i32,
    pub luks_method: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ComplianceDto {
    pub controller_version: String,
    pub crypto_library: String,
    pub tls13_cipher_suites: Vec<String>,
    pub tls12_cipher_suites: Vec<String>,
    pub kx_groups: Vec<String>,
    pub excluded_algorithms: Vec<String>,
    pub mtls_enabled: bool,
    pub access_control: Vec<AccessControlEntryDto>,
    pub total_nodes: i32,
    pub approved_nodes: i32,
    pub pending_nodes: i32,
    pub rejected_nodes: i32,
    pub total_vms: i32,
    pub running_vms: i32,
    pub stopped_vms: i32,
    pub total_networks: i32,
    pub nat_networks: i32,
    pub bridge_networks: i32,
    pub vxlan_networks: i32,
    pub sub_ca_enabled: bool,
    pub cert_auto_renewal_days: i32,
    pub nodes_expiring_30d: i32,
    pub nodes_cert_unknown: i32,
    pub nodes: Vec<NodeSummaryDto>,
    pub nodes_luks_tpm2: i32,
    pub nodes_luks_keyfile: i32,
    pub nodes_luks_unknown: i32,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct VolumeRowDto {
    pub name: String,
    pub role: String,
    pub attach_state: String,
    pub size: String,
    /// Attached VM name or id (whichever is available).
    pub vm: String,
    pub encrypted: bool,
    pub backend: String,
    pub pool: String,
    pub node_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct VmOperationRowDto {
    pub id: String,
    pub kind: String,
    pub phase: String,
    pub vm: String,
    pub cancel_requested: bool,
    pub send_succeeded: bool,
    pub source_node: String,
    pub target_node: String,
    pub started_at: String,
    pub finished_at: String,
    pub detail: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct VmRowDto {
    pub id: String,
    pub name: String,
    pub state: String,
    pub cpu: i32,
    pub memory: String,
    pub node_id: String,
    pub storage_backend: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct VmsPageDto {
    pub vms: Vec<VmRowDto>,
    pub page: u32,
    pub page_size: usize,
    pub total: usize,
    pub total_pages: u32,
    pub has_prev: bool,
    pub has_next: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct NetworkRowDto {
    pub name: String,
    pub network_type: String,
    pub node_id: String,
    pub external_ip: String,
    pub gateway_ip: String,
    pub internal_netmask: String,
    pub vlan_id: i32,
    pub enable_outbound_nat: bool,
    /// Prefix, or prefix plus gateway when both are set.
    pub ipv6: String,
    pub east_west_firewall: bool,
    pub forwarded_ports: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct HostInterfaceDto {
    pub name: String,
    pub mac_address: String,
    pub state: String,
    pub mtu: i32,
    pub addresses: Vec<String>,
    pub kind: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct NodeNetworkDto {
    pub node_id: String,
    pub hostname: String,
    pub address: String,
    pub gateway_interface: String,
    pub disable_vxlan: bool,
    pub interfaces: Vec<HostInterfaceDto>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct NetworkOverviewDto {
    pub default_gateway_interface: String,
    pub default_external_ip: String,
    pub default_gateway_ip: String,
    pub default_internal_netmask: String,
    pub nodes: Vec<NodeNetworkDto>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct StorageDiskRowDto {
    pub name: String,
    pub path: String,
    pub size: String,
    pub model: String,
    pub fstype: String,
    pub mountpoint: String,
    pub role_hint: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct NodeStorageDto {
    pub node_id: String,
    pub hostname: String,
    pub address: String,
    pub storage_backend: String,
    pub luks_method: String,
    pub disk_inventory_ok: bool,
    pub disks: Vec<StorageDiskRowDto>,
    pub lvm_inventory_ok: bool,
    pub lvm_volume_groups: Vec<LvmVolumeGroupDto>,
    pub lvm_logical_volumes: Vec<LvmLogicalVolumeDto>,
    pub lvm_physical_volumes: Vec<LvmPhysicalVolumeDto>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct StorageOverviewDto {
    pub approved_nodes: i32,
    pub nodes_disk_inventory_ok: i32,
    pub backend_filesystem_nodes: i32,
    pub backend_lvm_nodes: i32,
    pub backend_zfs_nodes: i32,
    pub backend_ceph_nodes: i32,
    pub backend_unspecified_nodes: i32,
    pub nodes_luks_tpm2: i32,
    pub nodes_luks_keyfile: i32,
    pub nodes_luks_unknown: i32,
    pub total_block_devices: i32,
    pub nodes: Vec<NodeStorageDto>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ReplicationOutgoingDto {
    pub peer_id: String,
    pub last_acked_event_id: i64,
    pub lag_events: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ReplicationIncomingDto {
    pub peer_endpoint: String,
    pub last_pulled_event_id: i64,
    pub last_applied_event_id: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ReplicationStatusDto {
    pub outbox_head_event_id: i64,
    pub outbox_size: i64,
    pub outgoing: Vec<ReplicationOutgoingDto>,
    pub incoming: Vec<ReplicationIncomingDto>,
    pub unresolved_conflicts: i64,
    pub pending_compensation_jobs: i64,
    pub failed_compensation_jobs: i64,
    pub materialization_backlog: i64,
    pub failed_reservations: i64,
    pub failed_retryable_reservations: i64,
    pub failed_non_retryable_reservations: i64,
    pub retry_exhausted_reservations: i64,
    pub zero_manual_slo_healthy: bool,
    pub zero_manual_slo_violations: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ReplicationConflictDto {
    pub id: i64,
    pub resource_key: String,
    pub incumbent_op_id: String,
    pub challenger_op_id: String,
    pub incumbent_controller_id: String,
    pub challenger_controller_id: String,
    pub reason: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AuditEventDto {
    pub id: i64,
    pub actor: String,
    pub action: String,
    pub resource: String,
    pub created_at: String,
    pub detail: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CephClusterRowDto {
    pub name: String,
    pub phase: String,
    pub health_message: String,
    pub public_network: String,
    pub cluster_network: String,
    pub size: i32,
    pub min_size: i32,
    pub encrypt_osds: bool,
    pub members: String,
    pub osd_devices: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
pub struct CephPgStateDto {
    pub state_name: String,
    pub count: i32,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
pub struct CephCheckDto {
    pub name: String,
    pub severity: String,
    pub message: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
pub struct CephHealthDetailDto {
    pub health_status: String,
    pub mon_quorum: String,
    pub osd_total: i32,
    pub osd_up: i32,
    pub osd_in: i32,
    pub pgs_total: i32,
    pub pgs_by_state: Vec<CephPgStateDto>,
    pub noout: bool,
    pub norebalance: bool,
    pub flags: Vec<String>,
    pub checks: Vec<CephCheckDto>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
pub struct CephMemberDto {
    pub node_id: String,
    pub mon_addr: String,
    pub cluster_addr: String,
    pub public_iface: String,
    pub cluster_iface: String,
    pub osd_devices: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
pub struct CephDeviceClassDto {
    pub device_class: String,
    pub total_bytes: i64,
    pub used_bytes: i64,
    pub avail_bytes: i64,
    pub total_human: String,
    pub used_human: String,
    pub avail_human: String,
    pub used_percent: i32,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
pub struct CephPoolUsageDto {
    pub name: String,
    pub stored_bytes: i64,
    pub stored_human: String,
    pub objects: i64,
    pub percent_used_bp: i32,
    pub max_avail_bytes: i64,
    pub max_avail_human: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
pub struct CephCapacityDto {
    pub total_bytes: i64,
    pub used_bytes: i64,
    pub avail_bytes: i64,
    pub total_human: String,
    pub used_human: String,
    pub avail_human: String,
    pub used_percent: i32,
    pub by_class: Vec<CephDeviceClassDto>,
    pub pools: Vec<CephPoolUsageDto>,
    /// `df`, `pgmap`, or `none`.
    pub source: String,
    pub queried_node: String,
    pub error: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CephClusterDetailDto {
    pub name: String,
    pub generation: i64,
    pub fsid: String,
    pub phase: String,
    pub health_message: String,
    pub public_network: String,
    pub cluster_network: String,
    pub size: i32,
    pub min_size: i32,
    pub encrypt_osds: bool,
    pub force_wipe: bool,
    pub created_at: String,
    pub updated_at: String,
    pub members: Vec<CephMemberDto>,
    pub health: CephHealthDetailDto,
    pub capacity: CephCapacityDto,
    pub cluster_pie_svg: String,
    pub class_pie_svg: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SharedFilesystemRowDto {
    pub name: String,
    pub ceph_cluster: String,
    pub phase: String,
    pub health_message: String,
    pub quota: String,
    pub clients: i32,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ObjectStoreRowDto {
    pub name: String,
    pub ceph_cluster: String,
    pub phase: String,
    pub health_message: String,
    pub port: i32,
    pub tls: bool,
    pub members: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct LvmVolumeGroupDto {
    pub name: String,
    pub size: String,
    pub free: String,
    pub attr: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct LvmLogicalVolumeDto {
    pub name: String,
    pub vg_name: String,
    pub size: String,
    pub attr: String,
    pub path: String,
    pub pool: String,
    pub data_percent: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct LvmPhysicalVolumeDto {
    pub name: String,
    pub vg_name: String,
    pub size: String,
    pub free: String,
    pub attr: String,
}

/// One DBMS managed by kcore Database Service. v1 is a PostgreSQL instance.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct DbmsRowDto {
    pub name: String,
    pub engine: String,
    pub database: String,
    pub package: String,
    pub port: i32,
    pub node_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct DbmsDetailDto {
    pub name: String,
    pub engine: String,
    pub database: String,
    pub package: String,
    pub port: i32,
    pub node_id: String,
    /// How clients reach the database.
    pub access: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct KubernetesNodeDto {
    pub node_id: String,
    pub role: String,
    pub advertise_address: String,
    pub iface: String,
    pub initial_cluster: bool,
    pub join_existing: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct KubernetesClusterRowDto {
    pub name: String,
    pub phase: String,
    pub message: String,
    pub cni: String,
    pub api_server_endpoint: String,
    pub control_planes: i32,
    pub workers: i32,
    pub ready_control_planes: i32,
    pub ready_workers: i32,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct KubernetesNodeHealthDto {
    pub node_id: String,
    pub kubernetes_status: String,
    pub kubelet_active: bool,
    pub etcd_active: bool,
    pub api_server_active: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct KubernetesClusterDetailDto {
    pub name: String,
    pub generation: i64,
    pub phase: String,
    pub message: String,
    pub last_updated: String,
    pub active_node_ids: String,
    pub cluster_cidr: String,
    pub service_cidr: String,
    pub cluster_dns: String,
    pub api_server_endpoint: String,
    pub cni: String,
    pub cilium_configured: bool,
    pub cilium_mode: String,
    pub cilium_mtu: i32,
    pub cilium_tunnel: String,
    pub cilium_auto_direct: bool,
    pub cilium_lb_pools: String,
    pub ha_configured: bool,
    pub ha_enabled: bool,
    pub ha_vip: String,
    pub ha_interface: String,
    pub ha_method: String,
    pub control_plane_nodes: Vec<KubernetesNodeDto>,
    pub worker_nodes: Vec<KubernetesNodeDto>,
    pub overall_health: String,
    pub health_ready_control_planes: i32,
    pub health_total_control_planes: i32,
    pub health_ready_workers: i32,
    pub health_total_workers: i32,
    pub etcd_healthy: bool,
    pub etcd_member_count: i32,
    pub api_server_reachable: bool,
    pub node_health: Vec<KubernetesNodeHealthDto>,
    /// Empty when the health RPC succeeded.
    pub health_error: String,
}
