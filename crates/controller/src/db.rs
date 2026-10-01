use std::sync::{Arc, Mutex};

use anyhow::{Context, Result};
use rusqlite::{params, Connection};

fn is_sqlite_memory_database_path(path: &str) -> bool {
    let lower = path.to_ascii_lowercase();
    if lower == ":memory:" {
        return true;
    }
    if lower.starts_with("file::memory:") {
        return true;
    }
    if let Some(q) = lower.find('?') {
        if lower[q..].contains("mode=memory") {
            return true;
        }
    }
    false
}

fn snapshot_temp_path() -> std::path::PathBuf {
    static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let n = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    std::env::temp_dir().join(format!("kcore-snapshot-{}-{n}.sqlite", std::process::id()))
}

fn validate_database_path(path: &str) -> Result<()> {
    crate::path_safety::assert_safe_path(path, "database path")
}

#[derive(Debug, Clone)]
pub struct Database {
    conn: Arc<Mutex<Connection>>,
}

#[derive(Debug, Clone)]
pub struct NodeRow {
    pub id: String,
    pub hostname: String,
    pub address: String,
    pub cpu_cores: i32,
    pub memory_bytes: i64,
    pub status: String,
    pub last_heartbeat: String,
    pub gateway_interface: String,
    pub cpu_used: i32,
    pub memory_used: i64,
    pub storage_backend: String,
    pub disable_vxlan: bool,
    pub approval_status: String,
    pub cert_expiry_days: i32,
    pub luks_method: String,
    pub dc_id: String,
}

/// Result of [`Database::apply_vm_runtime_state`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VmRuntimeApply {
    Missing,
    Unchanged,
    Changed(VmRuntimeChange),
}

/// A VM runtime state that was stored because it differed from the previous value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VmRuntimeChange {
    pub vm_id: String,
    pub name: String,
    pub node_id: String,
    pub previous: String,
    pub current: String,
}

#[derive(Debug, Clone)]
pub struct VmRow {
    pub id: String,
    pub name: String,
    pub cpu: i32,
    pub memory_bytes: i64,
    pub image_path: String,
    pub image_url: String,
    pub image_sha256: String,
    pub image_format: String,
    pub image_size: i64,
    pub network: String,
    pub auto_start: bool,
    pub node_id: String,
    #[allow(dead_code)]
    pub created_at: String,
    pub runtime_state: String,
    pub cloud_init_user_data: String,
    pub storage_backend: String,
    pub storage_size_bytes: i64,
    pub vm_ip: String,
    /// Comma-separated canonical PCI addresses (`0000:03:00.0`). Empty means none.
    pub pci_devices: String,
}

#[derive(Debug, Clone)]
pub struct NodeGpuRow {
    pub node_id: String,
    pub name: String,
    pub family: String,
    pub model: String,
    pub address: String,
    pub pci_devices: String,
    pub iommu_group: i32,
    pub assignable: bool,
    pub blocked_reason: String,
    /// `gpu`, `nic`, `nvme`, or `raw`. Empty means `gpu`.
    pub kind: String,
    /// `compute`, `platform`, or `passthrough`.
    pub role: String,
    pub class_code: String,
    pub characteristics: String,
    pub driver: String,
}

/// A NIC after the primary. Position 0 stays on `vms.network` / `vms.vm_ip`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VmNicRow {
    pub vm_id: String,
    pub position: i32,
    pub network: String,
    pub mac_address: String,
    pub model: String,
    pub ip_address: String,
}

#[derive(Debug, Clone)]
pub struct WorkloadRow {
    pub id: String,
    pub name: String,
    pub kind: String,
    pub node_id: String,
    pub runtime_state: String,
    pub desired_state: String,
    pub vm_id: String,
    pub container_image: String,
    pub network: String,
    pub storage_backend: String,
    pub storage_size_bytes: i64,
    pub created_at: String,
}

#[derive(Debug, Clone)]
pub struct NetworkRow {
    pub name: String,
    pub external_ip: String,
    pub gateway_ip: String,
    pub internal_netmask: String,
    pub node_id: String,
    pub allowed_tcp_ports: String,
    pub allowed_udp_ports: String,
    pub vlan_id: i32,
    pub network_type: String,
    pub enable_outbound_nat: bool,
    pub vni: i32,
    pub next_ip: i32,
}

/// One NixOS `services.postgresql` database. v1 keeps a single row per node.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PostgresqlRow {
    pub name: String,
    pub database_name: String,
    pub package: String,
    pub port: i32,
    pub node_id: String,
    pub created_at: String,
}

/// Optional IPv6 prefix and east-west filtering for one network on one node.
///
/// Missing rows mean both are off. `ipv6_next` is the next host id to hand
/// out (the gateway is host 1).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NetworkPolicyRow {
    pub node_id: String,
    pub name: String,
    pub east_west: bool,
    pub ipv6_prefix: String,
    pub ipv6_gateway: String,
    pub ipv6_next: i32,
}

impl NetworkPolicyRow {
    pub fn absent(node_id: &str, name: &str) -> Self {
        Self {
            node_id: node_id.to_string(),
            name: name.to_string(),
            east_west: false,
            ipv6_prefix: String::new(),
            ipv6_gateway: String::new(),
            ipv6_next: 2,
        }
    }

    pub fn is_set(&self) -> bool {
        self.east_west || !self.ipv6_prefix.is_empty()
    }
}

/// Static IPv6 address for one NIC. Position 0 is the primary NIC.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VmIpv6Row {
    pub vm_id: String,
    pub position: i32,
    pub address: String,
}

#[derive(Debug, Clone)]
pub struct SecurityGroupRow {
    pub name: String,
    pub description: String,
    pub created_at: String,
}

#[derive(Debug, Clone)]
pub struct SecurityGroupRuleRow {
    pub id: String,
    pub security_group: String,
    pub protocol: String,
    pub host_port: i32,
    pub target_port: i32,
    pub source_cidr: String,
    pub target_vm: String,
    pub enable_dnat: bool,
}

#[derive(Debug, Clone)]
pub struct SecurityGroupVmAttachmentRow {
    pub security_group: String,
    pub vm_id: String,
}

#[derive(Debug, Clone)]
pub struct DiskLayoutRow {
    pub name: String,
    pub node_id: String,
    pub generation: i64,
    pub layout_nix: String,
    pub evacuate: bool,
    pub created_at: String,
    pub updated_at: String,
}

#[derive(Debug, Clone)]
pub struct DiskLayoutStatusRow {
    pub name: String,
    pub observed_generation: i64,
    pub phase: String,
    pub refusal_reason: String,
    pub message: String,
    pub last_transition_at: String,
}

#[derive(Debug, Clone)]
pub struct CephClusterRow {
    pub name: String,
    pub generation: i64,
    pub spec_json: String,
    pub bootstrap_json: String,
    pub created_at: String,
    pub updated_at: String,
}

#[derive(Debug, Clone)]
pub struct CephClusterStatusRow {
    pub name: String,
    pub observed_generation: i64,
    pub phase: String,
    pub health_message: String,
    pub ceph_status_json: String,
    pub last_transition_at: String,
}

#[derive(Debug, Clone)]
pub struct SharedFilesystemRow {
    pub name: String,
    pub generation: i64,
    pub spec_json: String,
    pub created_at: String,
    pub updated_at: String,
}

#[derive(Debug, Clone)]
pub struct SharedFilesystemStatusRow {
    pub name: String,
    pub observed_generation: i64,
    pub phase: String,
    pub health_message: String,
    pub last_transition_at: String,
}

#[derive(Debug, Clone)]
pub struct ObjectStoreRow {
    pub name: String,
    pub generation: i64,
    pub spec_json: String,
    pub created_at: String,
    pub updated_at: String,
}

#[derive(Debug, Clone)]
pub struct ObjectStoreStatusRow {
    pub name: String,
    pub observed_generation: i64,
    pub phase: String,
    pub health_message: String,
    pub last_transition_at: String,
}

#[derive(Debug, Clone)]
pub struct ObjectUserRow {
    pub name: String,
    pub store_name: String,
    pub access_key: String,
    pub secret: String,
    pub created_at: String,
}

/// Cluster-scoped block volume (Ceph RBD in v1). A VM may have one `root`
/// volume and zero or more `data` volumes; detached volumes have an empty
/// `vm_id`. See `.cursor/design/volumes.md` (local, not in git).
#[derive(Debug, Clone)]
pub struct VolumeRow {
    pub id: String,
    pub name: String,
    pub vm_id: String,
    pub role: String,
    pub slot: i32,
    pub pool: String,
    pub image: String,
    pub size_bytes: i64,
    pub storage_class: String,
    pub attach_state: String,
    pub serial: String,
    pub source_json: String,
    pub guest_format_json: String,
    pub generation: i64,
    pub parent_snapshot_id: String,
    /// -1 means not yet observed in the guest.
    pub guest_visible_bytes: i64,
    pub guest_checked_at: String,
    /// Host-side LUKS over RBD (E2).
    pub encrypted: bool,
    /// Base64 AES-GCM wrap of the volume DEK (empty when not encrypted).
    pub wrapped_dek: String,
    pub created_at: String,
}

impl VolumeRow {
    pub const ROLE_ROOT: &'static str = "root";
    pub const ROLE_DATA: &'static str = "data";
    pub const ATTACH_DETACHED: &'static str = "detached";
    pub const ATTACH_ATTACHING: &'static str = "attaching";
    pub const ATTACH_ATTACHED: &'static str = "attached";
    pub const ATTACH_DETACHING: &'static str = "detaching";

    /// Stable virtio serial from a volume id (hex digits, up to 12 chars).
    pub fn serial_from_id(id: &str) -> String {
        let hex: String = id
            .chars()
            .filter(|c| c.is_ascii_hexdigit())
            .take(12)
            .collect();
        if hex.is_empty() {
            format!("vol{}", id.chars().take(8).collect::<String>())
        } else {
            hex.to_ascii_lowercase()
        }
    }

    pub fn new_root(vm_id: &str, vm_name: &str, size_bytes: i64) -> Self {
        let id = uuid::Uuid::new_v4().to_string();
        let name = format!("{vm_name}-root");
        Self {
            serial: Self::serial_from_id(&id),
            id,
            name,
            vm_id: vm_id.to_string(),
            role: Self::ROLE_ROOT.into(),
            slot: 0,
            pool: "kcore-vms".into(),
            image: format!("kcore-{vm_id}"),
            size_bytes,
            storage_class: "ceph".into(),
            attach_state: Self::ATTACH_ATTACHED.into(),
            source_json: "{}".into(),
            guest_format_json: String::new(),
            generation: 1,
            parent_snapshot_id: String::new(),
            guest_visible_bytes: -1,
            guest_checked_at: String::new(),
            encrypted: false,
            wrapped_dek: String::new(),
            created_at: String::new(),
        }
    }

    pub fn new_data(name: &str, size_bytes: i64) -> Self {
        let id = uuid::Uuid::new_v4().to_string();
        let image = format!("kcore-vol-{}", Self::serial_from_id(&id));
        Self {
            serial: Self::serial_from_id(&id),
            id,
            name: name.to_string(),
            vm_id: String::new(),
            role: Self::ROLE_DATA.into(),
            slot: 0,
            pool: "kcore-vms".into(),
            image,
            size_bytes,
            storage_class: "ceph".into(),
            attach_state: Self::ATTACH_DETACHED.into(),
            source_json: "{}".into(),
            guest_format_json: String::new(),
            generation: 1,
            parent_snapshot_id: String::new(),
            guest_visible_bytes: -1,
            guest_checked_at: String::new(),
            encrypted: false,
            wrapped_dek: String::new(),
            created_at: String::new(),
        }
    }

    pub fn is_attached(&self) -> bool {
        self.attach_state == Self::ATTACH_ATTACHED || self.attach_state == Self::ATTACH_ATTACHING
    }
}

/// Long-running per-VM action (live migrate, resize, …). At most one open
/// operation per vm_id (partial unique index on finished_at = '').
#[derive(Debug, Clone)]
pub struct VmOperationRow {
    pub id: String,
    pub vm_id: String,
    pub kind: String,
    pub phase: String,
    pub source_node: String,
    pub target_node: String,
    pub cancel_requested: bool,
    pub send_succeeded: bool,
    pub detail_json: String,
    pub started_at: String,
    pub updated_at: String,
    pub finished_at: String,
}

impl VmOperationRow {
    pub const KIND_LIVE_MIGRATE: &'static str = "live_migrate";
    pub const PHASE_PREPARING: &'static str = "Preparing";
    pub const PHASE_SENDING: &'static str = "Sending";
    pub const PHASE_WAITING: &'static str = "Waiting";
    pub const PHASE_REASSIGNED: &'static str = "Reassigned";
    pub const PHASE_FINALIZING_DEST: &'static str = "FinalizingDest";
    pub const PHASE_FINALIZING_SOURCE: &'static str = "FinalizingSource";
    pub const PHASE_DONE: &'static str = "Done";
    pub const PHASE_FAILED: &'static str = "Failed";
    pub const PHASE_CANCELLED: &'static str = "Cancelled";
}

#[derive(Debug, Clone)]
pub struct VolumeSnapshotRow {
    pub id: String,
    pub name: String,
    pub volume_id: String,
    pub rbd_snap: String,
    pub protected: bool,
    pub size_bytes: i64,
    pub consistency: String,
    pub created_at: String,
}

#[derive(Debug, Clone)]
pub struct SnapshotPolicyRow {
    pub name: String,
    pub selector_vm: String,
    pub selector_volume: String,
    pub schedule: String,
    pub keep: i32,
    pub enabled: bool,
    pub last_run_at: String,
    pub last_message: String,
    pub created_at: String,
    pub updated_at: String,
}

#[derive(Debug, Clone)]
pub struct ClusterUpdateRow {
    pub name: String,
    pub generation: i64,
    pub target_version: String,
    pub flake_ref: String,
    pub flake_rev: String,
    pub spec_json: String,
    pub phase: String,
    pub approval_status: String,
    pub created_at: String,
    pub updated_at: String,
}

#[derive(Debug, Clone)]
pub struct OperatorRow {
    pub name: String,
    pub cert_serial: String,
    pub created_at: i64,
    pub updated_at: i64,
}

#[derive(Debug, Clone)]
pub struct ClusterUpdateNodeRow {
    pub update_name: String,
    pub node_id: String,
    pub observed_generation: i64,
    pub phase: String,
    pub current_version: String,
    pub target_version: String,
    pub prepared_closure: String,
    pub current_generation: String,
    pub target_generation: String,
    pub requires_reboot: bool,
    pub last_error: String,
    pub last_transition_at: String,
}

#[derive(Debug, Clone)]
pub struct SecurityGroupNetworkAttachmentRow {
    pub security_group: String,
    pub network_name: String,
    pub node_id: String,
}

#[derive(Debug, Clone)]
pub struct ReplicationOutboxRow {
    pub id: i64,
    #[allow(dead_code)]
    pub created_at: String,
    pub event_type: String,
    pub resource_key: String,
    pub payload: Vec<u8>,
}

#[derive(Debug, Clone)]
pub struct ReplicationAckRow {
    pub peer_id: String,
    pub last_event_id: i64,
    pub updated_at: String,
}

#[derive(Debug, Clone)]
pub struct ReplicationResourceHeadRow {
    pub resource_key: String,
    pub last_op_id: String,
    pub last_logical_ts_unix_ms: i64,
    pub last_policy_priority: i32,
    pub last_intent_epoch: i64,
    pub last_validity: String,
    pub last_safety_class: String,
    pub last_controller_id: String,
    pub last_event_id: i64,
    pub last_event_type: String,
    pub last_body_json: String,
}

#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct ReplicationConflictRow {
    pub id: i64,
    pub resource_key: String,
    pub incumbent_op_id: String,
    pub challenger_op_id: String,
    pub incumbent_controller_id: String,
    pub challenger_controller_id: String,
    pub reason: String,
}

#[derive(Debug, Clone)]
pub struct ReplicationCompensationJobRow {
    pub id: i64,
    pub conflict_id: i64,
    pub resource_key: String,
    pub loser_op_id: String,
    pub loser_event_type: String,
    pub loser_body_json: String,
    pub status: String,
    pub attempts: i32,
}

#[derive(Debug, Clone)]
pub struct ReplicationMaterializedHeadRow {
    pub resource_key: String,
    pub last_op_id: String,
    pub last_event_type: String,
}

#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct ReplicationReservationRow {
    pub reservation_key: String,
    pub resource_key: String,
    pub op_id: String,
    pub status: String,
    pub error: String,
    pub retry_count: i32,
}

#[derive(Debug, Clone)]
pub struct ControllerPeerRow {
    pub controller_id: String,
    pub address: String,
    pub dc_id: String,
    pub last_seen_at: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuditEventRow {
    pub id: i64,
    pub actor: String,
    pub action: String,
    pub resource: String,
    pub created_at: String,
    pub detail: String,
}

pub const CERT_STATUS_ACTIVE: &str = "active";
pub const CERT_STATUS_ROTATED: &str = "rotated";
pub const CERT_STATUS_REVOKED: &str = "revoked";

/// One certificate the controller has issued (`issued_certificates`).
///
/// Timestamps are RFC3339 `%Y-%m-%dT%H:%M:%SZ` strings so lexicographic
/// comparison in SQL matches chronological order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IssuedCertRow {
    pub serial_hex: String,
    pub subject_cn: String,
    pub identity_kind: String,
    pub node_id: String,
    pub issuer_cn: String,
    pub fingerprint_sha256: String,
    pub not_before: String,
    pub not_after: String,
    pub issued_at: String,
    pub status: String,
    /// RFC 5280 reason code, or -1 when the certificate is not revoked.
    pub revocation_reason: i32,
    pub revoked_at: String,
}

/// Last CRL the controller signed (`crl_state`, single row).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CrlStateRow {
    pub crl_number: i64,
    pub this_update: String,
    pub next_update: String,
    pub crl_pem: String,
    pub crl_der: Vec<u8>,
    pub revoked_count: i32,
    pub issuer_fingerprint: String,
}

/// Schema version written by [`Database::migrate`].
pub const SCHEMA_VERSION: i32 = 49;

/// Largest controller snapshot accepted by backup and restore.
pub const MAX_CLUSTER_SNAPSHOT_BYTES: usize = 32 * 1024 * 1024;

impl Database {
    pub fn open(path: &str) -> Result<Self> {
        validate_database_path(path)?;
        if !is_sqlite_memory_database_path(path) {
            if let Some(parent) = std::path::Path::new(path).parent() {
                if !parent.as_os_str().is_empty() {
                    std::fs::create_dir_all(parent).with_context(|| {
                        format!(
                            "failed to create database parent directory {}",
                            parent.display()
                        )
                    })?;
                }
            }
        }
        let conn = Connection::open(path)?;
        conn.execute_batch("PRAGMA journal_mode=WAL; PRAGMA foreign_keys=ON;")?;
        let db = Self {
            conn: Arc::new(Mutex::new(conn)),
        };
        db.migrate()?;
        Ok(db)
    }

    fn lock_conn(&self) -> Result<std::sync::MutexGuard<'_, Connection>, rusqlite::Error> {
        self.conn.lock().map_err(|_| {
            rusqlite::Error::SqliteFailure(
                rusqlite::ffi::Error::new(1),
                Some("database mutex poisoned".to_string()),
            )
        })
    }

    fn migrate(&self) -> Result<()> {
        let conn = self.lock_conn()?;
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS schema_version (
                version INTEGER NOT NULL
            );
            CREATE TABLE IF NOT EXISTS nodes (
                id TEXT PRIMARY KEY,
                hostname TEXT NOT NULL,
                address TEXT NOT NULL,
                cpu_cores INTEGER NOT NULL DEFAULT 0,
                memory_bytes INTEGER NOT NULL DEFAULT 0,
                status TEXT NOT NULL DEFAULT 'unknown',
                last_heartbeat TEXT NOT NULL DEFAULT '',
                gateway_interface TEXT NOT NULL DEFAULT '',
                cpu_used INTEGER NOT NULL DEFAULT 0,
                memory_used INTEGER NOT NULL DEFAULT 0,
                storage_backend TEXT NOT NULL DEFAULT 'filesystem',
                disable_vxlan INTEGER NOT NULL DEFAULT 0,
                approval_status TEXT NOT NULL DEFAULT 'approved',
                cert_expiry_days INTEGER NOT NULL DEFAULT -1,
                luks_method TEXT NOT NULL DEFAULT ''
            );
            CREATE TABLE IF NOT EXISTS vms (
                id TEXT PRIMARY KEY,
                name TEXT NOT NULL,
                cpu INTEGER NOT NULL,
                memory_bytes INTEGER NOT NULL,
                image_path TEXT NOT NULL,
                image_url TEXT NOT NULL DEFAULT '',
                image_sha256 TEXT NOT NULL DEFAULT '',
                image_format TEXT NOT NULL DEFAULT 'raw',
                image_size INTEGER NOT NULL DEFAULT 8192,
                network TEXT NOT NULL DEFAULT 'default',
                auto_start INTEGER NOT NULL DEFAULT 1,
                node_id TEXT NOT NULL REFERENCES nodes(id),
                created_at TEXT NOT NULL DEFAULT (datetime('now')),
                runtime_state TEXT NOT NULL DEFAULT 'unknown',
                cloud_init_user_data TEXT NOT NULL DEFAULT '',
                storage_backend TEXT NOT NULL DEFAULT 'filesystem',
                storage_size_bytes INTEGER NOT NULL DEFAULT 0,
                vm_ip TEXT NOT NULL DEFAULT '',
                pci_devices TEXT NOT NULL DEFAULT ''
            );
            CREATE TABLE IF NOT EXISTS networks (
                name TEXT NOT NULL,
                external_ip TEXT NOT NULL,
                gateway_ip TEXT NOT NULL,
                internal_netmask TEXT NOT NULL DEFAULT '255.255.255.0',
                node_id TEXT NOT NULL REFERENCES nodes(id),
                allowed_tcp_ports TEXT NOT NULL DEFAULT '',
                allowed_udp_ports TEXT NOT NULL DEFAULT '',
                vlan_id INTEGER NOT NULL DEFAULT 0,
                network_type TEXT NOT NULL DEFAULT 'nat',
                enable_outbound_nat INTEGER NOT NULL DEFAULT 1,
                vni INTEGER NOT NULL DEFAULT 0,
                next_ip INTEGER NOT NULL DEFAULT 2,
                PRIMARY KEY (name, node_id)
            );
            CREATE TABLE IF NOT EXISTS node_labels (
                node_id TEXT NOT NULL REFERENCES nodes(id),
                label TEXT NOT NULL,
                PRIMARY KEY (node_id, label)
            );
            CREATE TABLE IF NOT EXISTS ssh_keys (
                name TEXT PRIMARY KEY,
                public_key TEXT NOT NULL,
                created_at TEXT NOT NULL DEFAULT (datetime('now'))
            );
            CREATE TABLE IF NOT EXISTS vm_ssh_keys (
                vm_id TEXT NOT NULL REFERENCES vms(id) ON DELETE CASCADE,
                key_name TEXT NOT NULL REFERENCES ssh_keys(name),
                PRIMARY KEY (vm_id, key_name)
            );
            CREATE TABLE IF NOT EXISTS workloads (
                id TEXT PRIMARY KEY,
                name TEXT NOT NULL,
                kind TEXT NOT NULL,
                node_id TEXT NOT NULL REFERENCES nodes(id),
                runtime_state TEXT NOT NULL DEFAULT 'unknown',
                desired_state TEXT NOT NULL DEFAULT 'running',
                vm_id TEXT NOT NULL DEFAULT '',
                container_image TEXT NOT NULL DEFAULT '',
                network TEXT NOT NULL DEFAULT 'default',
                storage_backend TEXT NOT NULL DEFAULT 'filesystem',
                storage_size_bytes INTEGER NOT NULL DEFAULT 0,
                created_at TEXT NOT NULL DEFAULT (datetime('now'))
            );
            CREATE TABLE IF NOT EXISTS security_groups (
                name TEXT PRIMARY KEY,
                description TEXT NOT NULL DEFAULT '',
                created_at TEXT NOT NULL DEFAULT (datetime('now'))
            );
            CREATE TABLE IF NOT EXISTS security_group_rules (
                id TEXT PRIMARY KEY,
                security_group TEXT NOT NULL REFERENCES security_groups(name) ON DELETE CASCADE,
                protocol TEXT NOT NULL,
                host_port INTEGER NOT NULL,
                target_port INTEGER NOT NULL,
                source_cidr TEXT NOT NULL DEFAULT '',
                target_vm TEXT NOT NULL DEFAULT '',
                enable_dnat INTEGER NOT NULL DEFAULT 0
            );
            CREATE TABLE IF NOT EXISTS security_group_vm_attachments (
                security_group TEXT NOT NULL REFERENCES security_groups(name) ON DELETE CASCADE,
                vm_id TEXT NOT NULL REFERENCES vms(id) ON DELETE CASCADE,
                PRIMARY KEY (security_group, vm_id)
            );
            CREATE TABLE IF NOT EXISTS security_group_network_attachments (
                security_group TEXT NOT NULL REFERENCES security_groups(name) ON DELETE CASCADE,
                network_name TEXT NOT NULL,
                node_id TEXT NOT NULL REFERENCES nodes(id) ON DELETE CASCADE,
                PRIMARY KEY (security_group, network_name, node_id)
            );
            CREATE TABLE IF NOT EXISTS disk_layouts (
                name TEXT PRIMARY KEY,
                node_id TEXT NOT NULL REFERENCES nodes(id) ON DELETE CASCADE,
                generation INTEGER NOT NULL DEFAULT 1,
                layout_nix TEXT NOT NULL,
                created_at TEXT NOT NULL DEFAULT (datetime('now')),
                updated_at TEXT NOT NULL DEFAULT (datetime('now'))
            );
            CREATE INDEX IF NOT EXISTS idx_disk_layouts_node ON disk_layouts(node_id);
            CREATE TABLE IF NOT EXISTS disk_layout_status (
                name TEXT PRIMARY KEY REFERENCES disk_layouts(name) ON DELETE CASCADE,
                observed_generation INTEGER NOT NULL DEFAULT 0,
                phase TEXT NOT NULL DEFAULT 'pending',
                refusal_reason TEXT NOT NULL DEFAULT '',
                message TEXT NOT NULL DEFAULT '',
                last_transition_at TEXT NOT NULL DEFAULT (datetime('now'))
            );",
        )?;

        let version = Self::schema_version(&conn);

        if version < 1 {
            let _ = conn.execute(
                "ALTER TABLE vms ADD COLUMN image_url TEXT NOT NULL DEFAULT ''",
                [],
            );
            let _ = conn.execute(
                "ALTER TABLE vms ADD COLUMN image_sha256 TEXT NOT NULL DEFAULT ''",
                [],
            );
            let _ = conn.execute(
                "ALTER TABLE vms ADD COLUMN image_format TEXT NOT NULL DEFAULT 'raw'",
                [],
            );
            let _ = conn.execute(
                "UPDATE vms
                 SET image_format = CASE
                     WHEN lower(image_path) LIKE '%.qcow2' OR lower(image_path) LIKE '%.qcow' THEN 'qcow2'
                     ELSE 'raw'
                 END
                 WHERE image_format IS NULL
                    OR image_format = ''
                    OR (image_format = 'raw' AND image_url != '' AND (lower(image_path) LIKE '%.qcow2' OR lower(image_path) LIKE '%.qcow'))",
                [],
            );
        }

        if version < 2 {
            let _ = conn.execute(
                "ALTER TABLE vms ADD COLUMN runtime_state TEXT NOT NULL DEFAULT 'unknown'",
                [],
            );
        }

        if version < 3 {
            let _ = conn.execute(
                "ALTER TABLE nodes ADD COLUMN cpu_used INTEGER NOT NULL DEFAULT 0",
                [],
            );
            let _ = conn.execute(
                "ALTER TABLE nodes ADD COLUMN memory_used INTEGER NOT NULL DEFAULT 0",
                [],
            );
            let _ = conn.execute(
                "ALTER TABLE networks ADD COLUMN allowed_tcp_ports TEXT NOT NULL DEFAULT ''",
                [],
            );
            let _ = conn.execute(
                "ALTER TABLE networks ADD COLUMN allowed_udp_ports TEXT NOT NULL DEFAULT ''",
                [],
            );
            let _ = conn.execute(
                "ALTER TABLE vms ADD COLUMN cloud_init_user_data TEXT NOT NULL DEFAULT ''",
                [],
            );
        }

        if version < 4 {
            let _ = conn.execute_batch(
                "CREATE TABLE IF NOT EXISTS ssh_keys (
                    name TEXT PRIMARY KEY,
                    public_key TEXT NOT NULL,
                    created_at TEXT NOT NULL DEFAULT (datetime('now'))
                );
                CREATE TABLE IF NOT EXISTS vm_ssh_keys (
                    vm_id TEXT NOT NULL REFERENCES vms(id) ON DELETE CASCADE,
                    key_name TEXT NOT NULL REFERENCES ssh_keys(name),
                    PRIMARY KEY (vm_id, key_name)
                );",
            );
        }

        if version < 5 {
            let _ = conn.execute(
                "ALTER TABLE nodes ADD COLUMN storage_backend TEXT NOT NULL DEFAULT 'filesystem'",
                [],
            );
            let _ = conn.execute(
                "ALTER TABLE vms ADD COLUMN storage_backend TEXT NOT NULL DEFAULT 'filesystem'",
                [],
            );
            let _ = conn.execute(
                "ALTER TABLE vms ADD COLUMN storage_size_bytes INTEGER NOT NULL DEFAULT 0",
                [],
            );
        }

        if version < 6 {
            let _ = conn.execute(
                "ALTER TABLE networks ADD COLUMN vlan_id INTEGER NOT NULL DEFAULT 0",
                [],
            );
        }

        if version < 7 {
            let _ = conn.execute(
                "ALTER TABLE networks ADD COLUMN network_type TEXT NOT NULL DEFAULT 'nat'",
                [],
            );
            let _ = conn.execute(
                "ALTER TABLE networks ADD COLUMN enable_outbound_nat INTEGER NOT NULL DEFAULT 1",
                [],
            );
            let _ = conn.execute(
                "ALTER TABLE networks ADD COLUMN vni INTEGER NOT NULL DEFAULT 0",
                [],
            );
            let _ = conn.execute(
                "ALTER TABLE networks ADD COLUMN next_ip INTEGER NOT NULL DEFAULT 2",
                [],
            );
            let _ = conn.execute(
                "ALTER TABLE vms ADD COLUMN vm_ip TEXT NOT NULL DEFAULT ''",
                [],
            );
        }

        if version < 8 {
            let _ = conn.execute(
                "ALTER TABLE nodes ADD COLUMN disable_vxlan INTEGER NOT NULL DEFAULT 0",
                [],
            );
        }

        if version < 9 {
            let _ = conn.execute(
                "ALTER TABLE nodes ADD COLUMN approval_status TEXT NOT NULL DEFAULT 'approved'",
                [],
            );
        }

        if version < 10 {
            let _ = conn.execute(
                "ALTER TABLE nodes ADD COLUMN cert_expiry_days INTEGER NOT NULL DEFAULT -1",
                [],
            );
        }

        if version < 11 {
            let _ = conn.execute_batch(
                "CREATE TABLE IF NOT EXISTS replication_outbox (
                    id INTEGER PRIMARY KEY AUTOINCREMENT,
                    created_at TEXT NOT NULL DEFAULT (datetime('now')),
                    event_type TEXT NOT NULL,
                    resource_key TEXT NOT NULL,
                    payload BLOB NOT NULL
                );",
            );
        }

        if version < 12 {
            let _ = conn.execute_batch(
                "CREATE TABLE IF NOT EXISTS replication_ack (
                    peer_id TEXT PRIMARY KEY,
                    last_event_id INTEGER NOT NULL DEFAULT 0,
                    updated_at TEXT NOT NULL DEFAULT (datetime('now'))
                );",
            );
        }

        if version < 13 {
            let _ = conn.execute_batch(
                "CREATE TABLE IF NOT EXISTS replication_received_ops (
                    op_id TEXT PRIMARY KEY,
                    origin_controller_id TEXT NOT NULL,
                    event_type TEXT NOT NULL,
                    resource_key TEXT NOT NULL,
                    first_seen_at TEXT NOT NULL DEFAULT (datetime('now'))
                );",
            );
        }

        if version < 14 {
            let _ = conn.execute_batch(
                "CREATE TABLE IF NOT EXISTS replication_resource_heads (
                    resource_key TEXT PRIMARY KEY,
                    last_op_id TEXT NOT NULL,
                    last_logical_ts_unix_ms INTEGER NOT NULL,
                    last_controller_id TEXT NOT NULL,
                    last_event_id INTEGER NOT NULL,
                    last_event_type TEXT NOT NULL,
                    last_body_json TEXT NOT NULL,
                    updated_at TEXT NOT NULL DEFAULT (datetime('now'))
                );",
            );
        }

        if version < 15 {
            let _ = conn.execute_batch(
                "CREATE TABLE IF NOT EXISTS replication_conflicts (
                    id INTEGER PRIMARY KEY AUTOINCREMENT,
                    resource_key TEXT NOT NULL,
                    incumbent_op_id TEXT NOT NULL,
                    challenger_op_id TEXT NOT NULL,
                    incumbent_controller_id TEXT NOT NULL,
                    challenger_controller_id TEXT NOT NULL,
                    reason TEXT NOT NULL,
                    resolved INTEGER NOT NULL DEFAULT 0,
                    created_at TEXT NOT NULL DEFAULT (datetime('now'))
                );",
            );
        }

        if version < 16 {
            let _ = conn.execute(
                "ALTER TABLE replication_resource_heads ADD COLUMN last_policy_priority INTEGER NOT NULL DEFAULT 0",
                [],
            );
            let _ = conn.execute(
                "ALTER TABLE replication_resource_heads ADD COLUMN last_intent_epoch INTEGER NOT NULL DEFAULT 0",
                [],
            );
            let _ = conn.execute(
                "ALTER TABLE replication_resource_heads ADD COLUMN last_validity TEXT NOT NULL DEFAULT 'valid'",
                [],
            );
            let _ = conn.execute(
                "ALTER TABLE replication_resource_heads ADD COLUMN last_safety_class TEXT NOT NULL DEFAULT 'safe'",
                [],
            );
        }

        if version < 17 {
            let _ = conn.execute_batch(
                "CREATE TABLE IF NOT EXISTS replication_compensation_jobs (
                    id INTEGER PRIMARY KEY AUTOINCREMENT,
                    conflict_id INTEGER NOT NULL REFERENCES replication_conflicts(id) ON DELETE CASCADE,
                    resource_key TEXT NOT NULL,
                    loser_op_id TEXT NOT NULL,
                    status TEXT NOT NULL DEFAULT 'pending',
                    attempts INTEGER NOT NULL DEFAULT 0,
                    last_error TEXT NOT NULL DEFAULT '',
                    created_at TEXT NOT NULL DEFAULT (datetime('now')),
                    updated_at TEXT NOT NULL DEFAULT (datetime('now'))
                );",
            );
        }

        if version < 18 {
            let _ = conn.execute_batch(
                "CREATE TABLE IF NOT EXISTS replication_materialized_heads (
                    resource_key TEXT PRIMARY KEY,
                    last_op_id TEXT NOT NULL,
                    last_event_type TEXT NOT NULL,
                    updated_at TEXT NOT NULL DEFAULT (datetime('now'))
                );",
            );
        }

        if version < 19 {
            let _ = conn.execute_batch(
                "CREATE TABLE IF NOT EXISTS replication_reservations (
                    reservation_key TEXT NOT NULL,
                    resource_key TEXT NOT NULL,
                    op_id TEXT NOT NULL,
                    status TEXT NOT NULL,
                    error TEXT NOT NULL DEFAULT '',
                    retry_count INTEGER NOT NULL DEFAULT 0,
                    updated_at TEXT NOT NULL DEFAULT (datetime('now')),
                    PRIMARY KEY (reservation_key, resource_key)
                );",
            );
        }

        if version < 20 {
            let _ = conn.execute(
                "ALTER TABLE replication_reservations ADD COLUMN retry_count INTEGER NOT NULL DEFAULT 0",
                [],
            );
        }

        if version < 21 {
            let _ = conn.execute(
                "ALTER TABLE nodes ADD COLUMN luks_method TEXT NOT NULL DEFAULT ''",
                [],
            );
        }

        if version < 22 {
            let _ = conn.execute_batch(
                "CREATE TABLE IF NOT EXISTS workloads (
                    id TEXT PRIMARY KEY,
                    name TEXT NOT NULL,
                    kind TEXT NOT NULL,
                    node_id TEXT NOT NULL REFERENCES nodes(id),
                    runtime_state TEXT NOT NULL DEFAULT 'unknown',
                    desired_state TEXT NOT NULL DEFAULT 'running',
                    vm_id TEXT NOT NULL DEFAULT '',
                    container_image TEXT NOT NULL DEFAULT '',
                    network TEXT NOT NULL DEFAULT 'default',
                    storage_backend TEXT NOT NULL DEFAULT 'filesystem',
                    storage_size_bytes INTEGER NOT NULL DEFAULT 0,
                    created_at TEXT NOT NULL DEFAULT (datetime('now'))
                );
                CREATE INDEX IF NOT EXISTS idx_workloads_node_id ON workloads(node_id);
                CREATE INDEX IF NOT EXISTS idx_workloads_kind ON workloads(kind);
                CREATE INDEX IF NOT EXISTS idx_workloads_runtime_state ON workloads(runtime_state);",
            );
        }
        if version < 23 {
            let _ = conn.execute_batch(
                "CREATE TABLE IF NOT EXISTS security_groups (
                    name TEXT PRIMARY KEY,
                    description TEXT NOT NULL DEFAULT '',
                    created_at TEXT NOT NULL DEFAULT (datetime('now'))
                );
                CREATE TABLE IF NOT EXISTS security_group_rules (
                    id TEXT PRIMARY KEY,
                    security_group TEXT NOT NULL REFERENCES security_groups(name) ON DELETE CASCADE,
                    protocol TEXT NOT NULL,
                    host_port INTEGER NOT NULL,
                    target_port INTEGER NOT NULL,
                    source_cidr TEXT NOT NULL DEFAULT '',
                    target_vm TEXT NOT NULL DEFAULT '',
                    enable_dnat INTEGER NOT NULL DEFAULT 0
                );
                CREATE TABLE IF NOT EXISTS security_group_vm_attachments (
                    security_group TEXT NOT NULL REFERENCES security_groups(name) ON DELETE CASCADE,
                    vm_id TEXT NOT NULL REFERENCES vms(id) ON DELETE CASCADE,
                    PRIMARY KEY (security_group, vm_id)
                );
                CREATE TABLE IF NOT EXISTS security_group_network_attachments (
                    security_group TEXT NOT NULL REFERENCES security_groups(name) ON DELETE CASCADE,
                    network_name TEXT NOT NULL,
                    node_id TEXT NOT NULL REFERENCES nodes(id) ON DELETE CASCADE,
                    PRIMARY KEY (security_group, network_name, node_id)
                );",
            );
        }

        if version < 24 {
            let _ = conn.execute(
                "ALTER TABLE replication_compensation_jobs ADD COLUMN loser_event_type TEXT NOT NULL DEFAULT ''",
                [],
            );
            let _ = conn.execute(
                "ALTER TABLE replication_compensation_jobs ADD COLUMN loser_body_json TEXT NOT NULL DEFAULT '{}'",
                [],
            );
        }

        if version < 25 {
            let _ = conn.execute_batch(
                "CREATE TABLE IF NOT EXISTS controller_peers (
                    controller_id TEXT PRIMARY KEY,
                    address TEXT NOT NULL,
                    dc_id TEXT NOT NULL DEFAULT 'DC1',
                    last_seen_at TEXT NOT NULL DEFAULT (datetime('now'))
                );",
            );
        }

        if version < 26 {
            let _ = conn.execute(
                "ALTER TABLE nodes ADD COLUMN dc_id TEXT NOT NULL DEFAULT ''",
                [],
            );
        }

        if version < 27 {
            let _ = conn.execute_batch(
                "CREATE TABLE IF NOT EXISTS disk_layouts (
                    name TEXT PRIMARY KEY,
                    node_id TEXT NOT NULL REFERENCES nodes(id) ON DELETE CASCADE,
                    generation INTEGER NOT NULL DEFAULT 1,
                    layout_nix TEXT NOT NULL,
                    created_at TEXT NOT NULL DEFAULT (datetime('now')),
                    updated_at TEXT NOT NULL DEFAULT (datetime('now'))
                );
                CREATE INDEX IF NOT EXISTS idx_disk_layouts_node ON disk_layouts(node_id);
                CREATE TABLE IF NOT EXISTS disk_layout_status (
                    name TEXT PRIMARY KEY REFERENCES disk_layouts(name) ON DELETE CASCADE,
                    observed_generation INTEGER NOT NULL DEFAULT 0,
                    phase TEXT NOT NULL DEFAULT 'pending',
                    refusal_reason TEXT NOT NULL DEFAULT '',
                    message TEXT NOT NULL DEFAULT '',
                    last_transition_at TEXT NOT NULL DEFAULT (datetime('now'))
                );",
            );
        }

        if version < 28 {
            let _ = conn.execute_batch(
                "CREATE TABLE IF NOT EXISTS cluster_updates (
                    name TEXT PRIMARY KEY,
                    generation INTEGER NOT NULL,
                    target_version TEXT NOT NULL,
                    flake_ref TEXT NOT NULL,
                    flake_rev TEXT NOT NULL,
                    spec_json TEXT NOT NULL,
                    phase TEXT NOT NULL,
                    approval_status TEXT NOT NULL,
                    created_at TEXT NOT NULL DEFAULT (datetime('now')),
                    updated_at TEXT NOT NULL DEFAULT (datetime('now'))
                );
                CREATE TABLE IF NOT EXISTS cluster_update_nodes (
                    update_name TEXT NOT NULL REFERENCES cluster_updates(name) ON DELETE CASCADE,
                    node_id TEXT NOT NULL REFERENCES nodes(id) ON DELETE CASCADE,
                    observed_generation INTEGER NOT NULL DEFAULT 0,
                    phase TEXT NOT NULL DEFAULT 'pending',
                    current_version TEXT NOT NULL DEFAULT '',
                    target_version TEXT NOT NULL DEFAULT '',
                    prepared_closure TEXT NOT NULL DEFAULT '',
                    current_generation TEXT NOT NULL DEFAULT '',
                    target_generation TEXT NOT NULL DEFAULT '',
                    requires_reboot INTEGER NOT NULL DEFAULT 0,
                    last_error TEXT NOT NULL DEFAULT '',
                    last_transition_at TEXT NOT NULL DEFAULT (datetime('now')),
                    PRIMARY KEY (update_name, node_id)
                );
                CREATE INDEX IF NOT EXISTS idx_cluster_update_nodes_update
                  ON cluster_update_nodes(update_name);
                CREATE TABLE IF NOT EXISTS cluster_update_events (
                    id INTEGER PRIMARY KEY AUTOINCREMENT,
                    update_name TEXT NOT NULL,
                    event_type TEXT NOT NULL,
                    body_json TEXT NOT NULL DEFAULT '{}',
                    created_at TEXT NOT NULL DEFAULT (datetime('now'))
                );",
            );
        }

        // Schema 29 is reserved for append-only audit_events (separate change).
        // Operator RBAC tables land at 30 so the two features can merge cleanly.
        if version < 30 {
            let _ = conn.execute_batch(
                "CREATE TABLE IF NOT EXISTS audit_events (
                    id INTEGER PRIMARY KEY AUTOINCREMENT,
                    actor TEXT NOT NULL,
                    action TEXT NOT NULL,
                    resource TEXT NOT NULL,
                    created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ','now')),
                    detail TEXT NOT NULL DEFAULT ''
                );
                CREATE INDEX IF NOT EXISTS idx_audit_events_created
                  ON audit_events(created_at DESC);
                CREATE INDEX IF NOT EXISTS idx_audit_events_action
                  ON audit_events(action);
                CREATE TRIGGER IF NOT EXISTS audit_events_no_update
                  BEFORE UPDATE ON audit_events
                  BEGIN
                    SELECT RAISE(ABORT, 'audit_events is append-only');
                  END;
                CREATE TRIGGER IF NOT EXISTS audit_events_no_delete
                  BEFORE DELETE ON audit_events
                  BEGIN
                    SELECT RAISE(ABORT, 'audit_events is append-only');
                  END;",
            );
        }

        if version < 30 {
            let _ = conn.execute_batch(
                "CREATE TABLE IF NOT EXISTS operators (
                    name TEXT PRIMARY KEY,
                    cert_serial TEXT NOT NULL DEFAULT '',
                    created_at INTEGER NOT NULL,
                    updated_at INTEGER NOT NULL
                );
                CREATE TABLE IF NOT EXISTS operator_roles (
                    operator_name TEXT NOT NULL REFERENCES operators(name) ON DELETE CASCADE,
                    role TEXT NOT NULL,
                    granted_at INTEGER NOT NULL,
                    PRIMARY KEY (operator_name, role)
                );",
            );
        }

        if version < 31 {
            conn.execute_batch(
                "CREATE TABLE IF NOT EXISTS ceph_clusters (
                    name TEXT PRIMARY KEY,
                    generation INTEGER NOT NULL DEFAULT 1,
                    spec_json TEXT NOT NULL,
                    created_at TEXT NOT NULL DEFAULT (datetime('now')),
                    updated_at TEXT NOT NULL DEFAULT (datetime('now'))
                );
                CREATE TABLE IF NOT EXISTS ceph_cluster_status (
                    name TEXT PRIMARY KEY REFERENCES ceph_clusters(name) ON DELETE CASCADE,
                    observed_generation INTEGER NOT NULL DEFAULT 0,
                    phase TEXT NOT NULL DEFAULT 'pending',
                    health_message TEXT NOT NULL DEFAULT '',
                    ceph_status_json TEXT NOT NULL DEFAULT '',
                    last_transition_at TEXT NOT NULL DEFAULT (datetime('now'))
                );
                CREATE TABLE IF NOT EXISTS volumes (
                    id TEXT PRIMARY KEY,
                    vm_id TEXT NOT NULL UNIQUE,
                    pool TEXT NOT NULL,
                    image TEXT NOT NULL,
                    size_bytes INTEGER NOT NULL,
                    created_at TEXT NOT NULL DEFAULT (datetime('now'))
                );
                CREATE INDEX IF NOT EXISTS idx_volumes_vm ON volumes(vm_id);",
            )?;
        }

        if version < 32 {
            let _ = conn.execute(
                "ALTER TABLE ceph_clusters ADD COLUMN bootstrap_json TEXT NOT NULL DEFAULT ''",
                [],
            );
        }

        // Certificate inventory + signed CRL state (docs/mtls-bootstrap-and-auth.md §4).
        // `serial_hex` is uppercase hex without separators so it matches
        // `openssl x509 -serial` output and can be compared to the serial
        // extracted from a peer certificate without normalisation.
        if version < 33 {
            conn.execute_batch(
                "CREATE TABLE IF NOT EXISTS issued_certificates (
                    serial_hex TEXT PRIMARY KEY,
                    subject_cn TEXT NOT NULL,
                    identity_kind TEXT NOT NULL DEFAULT 'node',
                    node_id TEXT NOT NULL DEFAULT '',
                    issuer_cn TEXT NOT NULL DEFAULT '',
                    fingerprint_sha256 TEXT NOT NULL DEFAULT '',
                    not_before TEXT NOT NULL,
                    not_after TEXT NOT NULL,
                    issued_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%SZ','now')),
                    status TEXT NOT NULL DEFAULT 'active',
                    revocation_reason INTEGER NOT NULL DEFAULT -1,
                    revoked_at TEXT NOT NULL DEFAULT ''
                );
                CREATE INDEX IF NOT EXISTS idx_issued_certs_status
                  ON issued_certificates(status);
                CREATE INDEX IF NOT EXISTS idx_issued_certs_not_after
                  ON issued_certificates(not_after);
                CREATE INDEX IF NOT EXISTS idx_issued_certs_node
                  ON issued_certificates(node_id);
                CREATE INDEX IF NOT EXISTS idx_issued_certs_subject
                  ON issued_certificates(subject_cn);
                -- Single row (id = 1) holding the last signed CRL so crl_number
                -- stays monotonic across controller restarts.
                CREATE TABLE IF NOT EXISTS crl_state (
                    id INTEGER PRIMARY KEY CHECK (id = 1),
                    crl_number INTEGER NOT NULL DEFAULT 0,
                    this_update TEXT NOT NULL DEFAULT '',
                    next_update TEXT NOT NULL DEFAULT '',
                    crl_pem TEXT NOT NULL DEFAULT '',
                    crl_der BLOB NOT NULL DEFAULT x'',
                    revoked_count INTEGER NOT NULL DEFAULT 0,
                    issuer_fingerprint TEXT NOT NULL DEFAULT ''
                );",
            )?;
        }

        if version < 34 {
            let _ = conn.execute(
                "ALTER TABLE vms ADD COLUMN pci_devices TEXT NOT NULL DEFAULT ''",
                [],
            );
        }

        if version < 35 {
            conn.execute_batch(
                "CREATE TABLE IF NOT EXISTS node_gpus (
                    node_id TEXT NOT NULL REFERENCES nodes(id) ON DELETE CASCADE,
                    name TEXT NOT NULL,
                    family TEXT NOT NULL,
                    model TEXT NOT NULL,
                    address TEXT NOT NULL,
                    pci_devices TEXT NOT NULL,
                    iommu_group INTEGER NOT NULL DEFAULT -1,
                    assignable INTEGER NOT NULL DEFAULT 0,
                    blocked_reason TEXT NOT NULL DEFAULT '',
                    PRIMARY KEY (node_id, name)
                );
                CREATE TABLE IF NOT EXISTS vm_gpu_bindings (
                    vm_id TEXT PRIMARY KEY REFERENCES vms(id) ON DELETE CASCADE,
                    gpu_name TEXT NOT NULL
                );",
            )?;
        }

        if version < 36 {
            conn.execute_batch(
                "ALTER TABLE node_gpus ADD COLUMN kind TEXT NOT NULL DEFAULT 'gpu';
                ALTER TABLE node_gpus ADD COLUMN role TEXT NOT NULL DEFAULT '';
                ALTER TABLE node_gpus ADD COLUMN class_code TEXT NOT NULL DEFAULT '';
                ALTER TABLE node_gpus ADD COLUMN characteristics TEXT NOT NULL DEFAULT '';
                ALTER TABLE node_gpus ADD COLUMN driver TEXT NOT NULL DEFAULT '';",
            )?;
        }

        if version < 37 {
            conn.execute_batch(
                "CREATE TABLE IF NOT EXISTS postgresql_instances (
                    name TEXT PRIMARY KEY,
                    database_name TEXT NOT NULL,
                    package TEXT NOT NULL,
                    port INTEGER NOT NULL,
                    node_id TEXT NOT NULL REFERENCES nodes(id),
                    created_at TEXT NOT NULL DEFAULT (datetime('now')),
                    UNIQUE(node_id)
                );",
            )?;
        }

        if version < 38 {
            conn.execute_batch(
                "CREATE TABLE IF NOT EXISTS ceph_retired_nodes (
                    cluster_name TEXT NOT NULL,
                    node_id TEXT NOT NULL,
                    PRIMARY KEY (cluster_name, node_id)
                );",
            )?;
        }

        // First-class volumes: many per VM, detachable data disks (schema 39).
        if version < 39 {
            conn.execute_batch(
                "CREATE TABLE IF NOT EXISTS volumes_v35 (
                    id TEXT PRIMARY KEY,
                    name TEXT NOT NULL UNIQUE,
                    vm_id TEXT NOT NULL DEFAULT '',
                    role TEXT NOT NULL DEFAULT 'root',
                    slot INTEGER NOT NULL DEFAULT 0,
                    pool TEXT NOT NULL,
                    image TEXT NOT NULL,
                    size_bytes INTEGER NOT NULL,
                    storage_class TEXT NOT NULL DEFAULT 'ceph',
                    attach_state TEXT NOT NULL DEFAULT 'attached',
                    serial TEXT NOT NULL DEFAULT '',
                    source_json TEXT NOT NULL DEFAULT '{}',
                    guest_format_json TEXT NOT NULL DEFAULT '',
                    generation INTEGER NOT NULL DEFAULT 1,
                    created_at TEXT NOT NULL DEFAULT (datetime('now'))
                );
                INSERT OR IGNORE INTO volumes_v35 (
                    id, name, vm_id, role, slot, pool, image, size_bytes,
                    storage_class, attach_state, serial, source_json,
                    guest_format_json, generation, created_at
                )
                SELECT
                    v.id,
                    COALESCE(
                        (SELECT name FROM vms WHERE id = v.vm_id) || '-root',
                        v.image || '-root'
                    ),
                    v.vm_id,
                    'root',
                    0,
                    v.pool,
                    v.image,
                    v.size_bytes,
                    'ceph',
                    'attached',
                    lower(substr(replace(v.id, '-', ''), 1, 12)),
                    '{}',
                    '',
                    1,
                    v.created_at
                FROM volumes v;
                DROP TABLE IF EXISTS volumes;
                ALTER TABLE volumes_v35 RENAME TO volumes;
                CREATE INDEX IF NOT EXISTS idx_volumes_vm ON volumes(vm_id);
                CREATE INDEX IF NOT EXISTS idx_volumes_attach ON volumes(attach_state);",
            )?;
        }

        if version < 40 {
            conn.execute_batch(
                "CREATE TABLE IF NOT EXISTS vm_operations (
                    id TEXT PRIMARY KEY,
                    vm_id TEXT NOT NULL,
                    kind TEXT NOT NULL,
                    phase TEXT NOT NULL,
                    source_node TEXT NOT NULL DEFAULT '',
                    target_node TEXT NOT NULL DEFAULT '',
                    cancel_requested INTEGER NOT NULL DEFAULT 0,
                    send_succeeded INTEGER NOT NULL DEFAULT 0,
                    detail_json TEXT NOT NULL DEFAULT '{}',
                    started_at TEXT NOT NULL DEFAULT (datetime('now')),
                    updated_at TEXT NOT NULL DEFAULT (datetime('now')),
                    finished_at TEXT NOT NULL DEFAULT ''
                );
                CREATE INDEX IF NOT EXISTS idx_vm_operations_vm ON vm_operations(vm_id);
                CREATE UNIQUE INDEX IF NOT EXISTS idx_vm_operations_open_vm
                  ON vm_operations(vm_id) WHERE finished_at = '';",
            )?;
        }

        if version < 41 {
            conn.execute_batch(
                "ALTER TABLE volumes ADD COLUMN parent_snapshot_id TEXT NOT NULL DEFAULT '';
                 CREATE TABLE IF NOT EXISTS volume_snapshots (
                    id TEXT PRIMARY KEY,
                    name TEXT NOT NULL UNIQUE,
                    volume_id TEXT NOT NULL,
                    rbd_snap TEXT NOT NULL,
                    protected INTEGER NOT NULL DEFAULT 1,
                    size_bytes INTEGER NOT NULL DEFAULT 0,
                    consistency TEXT NOT NULL DEFAULT 'crash',
                    created_at TEXT NOT NULL DEFAULT (datetime('now'))
                 );
                 CREATE INDEX IF NOT EXISTS idx_volume_snapshots_volume
                   ON volume_snapshots(volume_id);",
            )?;
        }

        if version < 42 {
            // guest_visible_bytes: -1 = unknown (not yet checked)
            conn.execute_batch(
                "ALTER TABLE volumes ADD COLUMN guest_visible_bytes INTEGER NOT NULL DEFAULT -1;
                 ALTER TABLE volumes ADD COLUMN guest_checked_at TEXT NOT NULL DEFAULT '';
                 CREATE TABLE IF NOT EXISTS snapshot_policies (
                    name TEXT PRIMARY KEY,
                    selector_vm TEXT NOT NULL DEFAULT '',
                    selector_volume TEXT NOT NULL DEFAULT '',
                    schedule TEXT NOT NULL,
                    keep INTEGER NOT NULL DEFAULT 7,
                    enabled INTEGER NOT NULL DEFAULT 1,
                    last_run_at TEXT NOT NULL DEFAULT '',
                    last_message TEXT NOT NULL DEFAULT '',
                    created_at TEXT NOT NULL DEFAULT (datetime('now')),
                    updated_at TEXT NOT NULL DEFAULT (datetime('now'))
                 );",
            )?;
        }

        if version < 43 {
            conn.execute_batch(
                "ALTER TABLE volumes ADD COLUMN encrypted INTEGER NOT NULL DEFAULT 0;
                 ALTER TABLE volumes ADD COLUMN wrapped_dek TEXT NOT NULL DEFAULT '';
                 CREATE TABLE IF NOT EXISTS guest_ops_node_keys (
                    node_id TEXT PRIMARY KEY,
                    public_key TEXT NOT NULL,
                    updated_at TEXT NOT NULL DEFAULT (datetime('now'))
                 );",
            )?;
        }

        if version < 44 {
            conn.execute_batch(
                "CREATE TABLE IF NOT EXISTS shared_filesystems (
                    name TEXT PRIMARY KEY,
                    generation INTEGER NOT NULL DEFAULT 1,
                    spec_json TEXT NOT NULL,
                    created_at TEXT NOT NULL DEFAULT (datetime('now')),
                    updated_at TEXT NOT NULL DEFAULT (datetime('now'))
                );
                CREATE TABLE IF NOT EXISTS shared_filesystem_status (
                    name TEXT PRIMARY KEY REFERENCES shared_filesystems(name) ON DELETE CASCADE,
                    observed_generation INTEGER NOT NULL DEFAULT 0,
                    phase TEXT NOT NULL DEFAULT 'pending',
                    health_message TEXT NOT NULL DEFAULT '',
                    last_transition_at TEXT NOT NULL DEFAULT (datetime('now'))
                );
                CREATE TABLE IF NOT EXISTS object_stores (
                    name TEXT PRIMARY KEY,
                    generation INTEGER NOT NULL DEFAULT 1,
                    spec_json TEXT NOT NULL,
                    created_at TEXT NOT NULL DEFAULT (datetime('now')),
                    updated_at TEXT NOT NULL DEFAULT (datetime('now'))
                );
                CREATE TABLE IF NOT EXISTS object_store_status (
                    name TEXT PRIMARY KEY REFERENCES object_stores(name) ON DELETE CASCADE,
                    observed_generation INTEGER NOT NULL DEFAULT 0,
                    phase TEXT NOT NULL DEFAULT 'pending',
                    health_message TEXT NOT NULL DEFAULT '',
                    last_transition_at TEXT NOT NULL DEFAULT (datetime('now'))
                );
                CREATE TABLE IF NOT EXISTS object_users (
                    name TEXT NOT NULL,
                    store_name TEXT NOT NULL REFERENCES object_stores(name) ON DELETE CASCADE,
                    access_key TEXT NOT NULL,
                    secret TEXT NOT NULL,
                    created_at TEXT NOT NULL DEFAULT (datetime('now')),
                    PRIMARY KEY (name, store_name)
                );
                CREATE INDEX IF NOT EXISTS idx_object_users_store ON object_users(store_name);",
            )?;
        }

        if version < 45 {
            let _ = conn.execute(
                "ALTER TABLE disk_layouts ADD COLUMN evacuate INTEGER NOT NULL DEFAULT 0",
                [],
            );
        }

        if version < 46 {
            conn.execute_batch(
                "CREATE TABLE IF NOT EXISTS vm_nics (
                    vm_id TEXT NOT NULL REFERENCES vms(id) ON DELETE CASCADE,
                    position INTEGER NOT NULL,
                    network TEXT NOT NULL,
                    mac_address TEXT NOT NULL DEFAULT '',
                    model TEXT NOT NULL DEFAULT 'virtio',
                    ip_address TEXT NOT NULL DEFAULT '',
                    PRIMARY KEY (vm_id, position)
                );",
            )?;
        }

        if version < 47 {
            conn.execute_batch(
                "CREATE TABLE IF NOT EXISTS vm_placement (
                    vm_id TEXT PRIMARY KEY REFERENCES vms(id) ON DELETE CASCADE,
                    anti_affinity TEXT NOT NULL DEFAULT ''
                );",
            )?;
        }

        if version < 48 {
            conn.execute_batch(
                "CREATE TABLE IF NOT EXISTS network_policy (
                    node_id TEXT NOT NULL,
                    name TEXT NOT NULL,
                    east_west INTEGER NOT NULL DEFAULT 0,
                    ipv6_prefix TEXT NOT NULL DEFAULT '',
                    ipv6_gateway TEXT NOT NULL DEFAULT '',
                    ipv6_next INTEGER NOT NULL DEFAULT 2,
                    PRIMARY KEY (node_id, name)
                );
                CREATE TABLE IF NOT EXISTS vm_ipv6 (
                    vm_id TEXT NOT NULL REFERENCES vms(id) ON DELETE CASCADE,
                    position INTEGER NOT NULL,
                    address TEXT NOT NULL,
                    PRIMARY KEY (vm_id, position)
                );
                CREATE TABLE IF NOT EXISTS vxlan_released_ips (
                    network TEXT NOT NULL,
                    ip TEXT NOT NULL,
                    PRIMARY KEY (network, ip)
                );
                CREATE TABLE IF NOT EXISTS ipv6_released (
                    scope TEXT NOT NULL,
                    address TEXT NOT NULL,
                    PRIMARY KEY (scope, address)
                );",
            )?;
        }

        if version < 49 {
            conn.execute_batch(
                "CREATE TABLE IF NOT EXISTS pending_node_config_push (
                    node_id TEXT PRIMARY KEY REFERENCES nodes(id) ON DELETE CASCADE
                );",
            )?;
        }

        const CURRENT_VERSION: i32 = SCHEMA_VERSION;
        if version < CURRENT_VERSION {
            conn.execute("DELETE FROM schema_version", [])?;
            conn.execute(
                "INSERT INTO schema_version (version) VALUES (?1)",
                params![CURRENT_VERSION],
            )?;
        }

        Ok(())
    }

    fn schema_version(conn: &Connection) -> i32 {
        conn.query_row("SELECT version FROM schema_version", [], |row| row.get(0))
            .unwrap_or(0)
    }

    /// Consistent SQLite snapshot of this database, and the schema version it carries.
    pub fn snapshot_sqlite(&self) -> Result<(Vec<u8>, i32), String> {
        let conn = self.lock_conn().map_err(|e| e.to_string())?;
        let version = Self::schema_version(&conn);
        let path = snapshot_temp_path();
        conn.backup(
            rusqlite::DatabaseName::Main,
            &path,
            None::<fn(rusqlite::backup::Progress)>,
        )
        .map_err(|e| format!("sqlite backup: {e}"))?;
        drop(conn);
        let bytes = std::fs::read(&path).map_err(|e| format!("reading snapshot: {e}"));
        let _ = std::fs::remove_file(&path);
        let bytes = bytes?;
        if bytes.len() > MAX_CLUSTER_SNAPSHOT_BYTES {
            return Err(format!(
                "snapshot is {} bytes, above the {MAX_CLUSTER_SNAPSHOT_BYTES} byte limit",
                bytes.len()
            ));
        }
        Ok((bytes, version))
    }

    /// Replace this database with a snapshot, then run migrations up to [`SCHEMA_VERSION`].
    pub fn restore_sqlite(&self, bytes: &[u8]) -> Result<i32, String> {
        if bytes.is_empty() {
            return Err("snapshot is empty".to_string());
        }
        if bytes.len() > MAX_CLUSTER_SNAPSHOT_BYTES {
            return Err(format!(
                "snapshot is {} bytes, above the {MAX_CLUSTER_SNAPSHOT_BYTES} byte limit",
                bytes.len()
            ));
        }
        if !bytes.starts_with(b"SQLite format 3\0") {
            return Err("snapshot is not a SQLite database".to_string());
        }
        let path = snapshot_temp_path();
        std::fs::write(&path, bytes).map_err(|e| format!("writing snapshot: {e}"))?;
        let restored = (|| {
            let src = Connection::open(&path).map_err(|e| format!("opening snapshot: {e}"))?;
            let integrity: String = src
                .query_row("PRAGMA integrity_check", [], |row| row.get(0))
                .map_err(|e| format!("integrity check: {e}"))?;
            if integrity != "ok" {
                return Err(format!("snapshot failed integrity check: {integrity}"));
            }
            let version = Self::schema_version(&src);
            if !(1..=SCHEMA_VERSION).contains(&version) {
                return Err(format!(
                    "snapshot schema version {version} is outside 1..={SCHEMA_VERSION}"
                ));
            }
            drop(src);
            {
                let mut conn = self.lock_conn().map_err(|e| e.to_string())?;
                conn.restore(
                    rusqlite::DatabaseName::Main,
                    &path,
                    None::<fn(rusqlite::backup::Progress)>,
                )
                .map_err(|e| format!("sqlite restore: {e}"))?;
            }
            self.migrate()
                .map_err(|e| format!("migrate after restore: {e}"))?;
            Ok(version)
        })();
        let _ = std::fs::remove_file(&path);
        restored
    }

    pub fn mark_node_config_push(&self, node_id: &str) -> Result<(), rusqlite::Error> {
        let conn = self.lock_conn()?;
        conn.execute(
            "INSERT INTO pending_node_config_push (node_id) VALUES (?1)
             ON CONFLICT(node_id) DO NOTHING",
            params![node_id],
        )?;
        Ok(())
    }

    pub fn node_needs_config_push(&self, node_id: &str) -> Result<bool, rusqlite::Error> {
        let conn = self.lock_conn()?;
        conn.query_row(
            "SELECT COUNT(*) FROM pending_node_config_push WHERE node_id = ?1",
            params![node_id],
            |row| row.get::<_, i64>(0),
        )
        .map(|count| count > 0)
    }

    pub fn clear_node_config_push(&self, node_id: &str) -> Result<(), rusqlite::Error> {
        let conn = self.lock_conn()?;
        conn.execute(
            "DELETE FROM pending_node_config_push WHERE node_id = ?1",
            params![node_id],
        )?;
        Ok(())
    }

    pub fn append_replication_outbox(
        &self,
        event_type: &str,
        resource_key: &str,
        payload: &[u8],
    ) -> Result<i64, rusqlite::Error> {
        let conn = self.lock_conn()?;
        conn.execute(
            "INSERT INTO replication_outbox (event_type, resource_key, payload) VALUES (?1, ?2, ?3)",
            params![event_type, resource_key, payload],
        )?;
        Ok(conn.last_insert_rowid())
    }

    pub fn append_audit_event(
        &self,
        actor: &str,
        action: &str,
        resource: &str,
        detail: &str,
    ) -> Result<i64, rusqlite::Error> {
        let conn = self.lock_conn()?;
        conn.execute(
            "INSERT INTO audit_events (actor, action, resource, detail) VALUES (?1, ?2, ?3, ?4)",
            params![actor, action, resource, detail],
        )?;
        Ok(conn.last_insert_rowid())
    }

    /// Newest-first. `limit` is clamped to 1..=1000 (callers may pass 0 for default 100).
    pub fn list_audit_events(
        &self,
        limit: u32,
        since: Option<&str>,
        action: Option<&str>,
    ) -> Result<Vec<AuditEventRow>, rusqlite::Error> {
        let limit = match limit {
            0 => 100u32,
            n => n.min(1000),
        };
        let conn = self.lock_conn()?;
        let since = since.map(str::trim).filter(|s| !s.is_empty());
        let action = action.map(str::trim).filter(|s| !s.is_empty());
        let mut rows = Vec::new();
        match (since, action) {
            (None, None) => {
                let mut stmt = conn.prepare(
                    "SELECT id, actor, action, resource, created_at, detail
                     FROM audit_events
                     ORDER BY id DESC
                     LIMIT ?1",
                )?;
                let iter = stmt.query_map(params![limit], map_audit_event_row)?;
                for row in iter {
                    rows.push(row?);
                }
            }
            (Some(since), None) => {
                let mut stmt = conn.prepare(
                    "SELECT id, actor, action, resource, created_at, detail
                     FROM audit_events
                     WHERE created_at >= ?1
                     ORDER BY id DESC
                     LIMIT ?2",
                )?;
                let iter = stmt.query_map(params![since, limit], map_audit_event_row)?;
                for row in iter {
                    rows.push(row?);
                }
            }
            (None, Some(action)) => {
                let mut stmt = conn.prepare(
                    "SELECT id, actor, action, resource, created_at, detail
                     FROM audit_events
                     WHERE action = ?1
                     ORDER BY id DESC
                     LIMIT ?2",
                )?;
                let iter = stmt.query_map(params![action, limit], map_audit_event_row)?;
                for row in iter {
                    rows.push(row?);
                }
            }
            (Some(since), Some(action)) => {
                let mut stmt = conn.prepare(
                    "SELECT id, actor, action, resource, created_at, detail
                     FROM audit_events
                     WHERE created_at >= ?1 AND action = ?2
                     ORDER BY id DESC
                     LIMIT ?3",
                )?;
                let iter = stmt.query_map(params![since, action, limit], map_audit_event_row)?;
                for row in iter {
                    rows.push(row?);
                }
            }
        }
        Ok(rows)
    }

    pub fn replication_outbox_len(&self) -> Result<i64, rusqlite::Error> {
        let conn = self.lock_conn()?;
        conn.query_row("SELECT COUNT(*) FROM replication_outbox", [], |row| {
            row.get(0)
        })
    }

    pub fn replication_outbox_head_id(&self) -> Result<i64, rusqlite::Error> {
        let conn = self.lock_conn()?;
        conn.query_row(
            "SELECT COALESCE(MAX(id), 0) FROM replication_outbox",
            [],
            |row| row.get(0),
        )
    }

    pub fn list_replication_outbox_since(
        &self,
        min_id_exclusive: i64,
        limit: i64,
    ) -> Result<Vec<ReplicationOutboxRow>, rusqlite::Error> {
        let conn = self.lock_conn()?;
        let mut stmt = conn.prepare(
            "SELECT id, created_at, event_type, resource_key, payload
             FROM replication_outbox
             WHERE id > ?1
             ORDER BY id ASC
             LIMIT ?2",
        )?;
        let rows = stmt.query_map(params![min_id_exclusive, limit], |row| {
            Ok(ReplicationOutboxRow {
                id: row.get(0)?,
                created_at: row.get(1)?,
                event_type: row.get(2)?,
                resource_key: row.get(3)?,
                payload: row.get(4)?,
            })
        })?;
        rows.collect()
    }

    pub fn upsert_replication_ack(
        &self,
        peer_id: &str,
        last_event_id: i64,
    ) -> Result<(), rusqlite::Error> {
        let conn = self.lock_conn()?;
        conn.execute(
            "INSERT INTO replication_ack (peer_id, last_event_id, updated_at)
             VALUES (?1, ?2, datetime('now'))
             ON CONFLICT(peer_id) DO UPDATE SET
               last_event_id=MAX(replication_ack.last_event_id, excluded.last_event_id),
                updated_at=datetime('now')",
            params![peer_id, last_event_id],
        )?;
        Ok(())
    }

    pub fn get_replication_ack(&self, peer_id: &str) -> Result<Option<i64>, rusqlite::Error> {
        let conn = self.lock_conn()?;
        match conn.query_row(
            "SELECT last_event_id FROM replication_ack WHERE peer_id = ?1",
            params![peer_id],
            |row| row.get(0),
        ) {
            Ok(v) => Ok(Some(v)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(e),
        }
    }

    pub fn list_replication_acks(&self) -> Result<Vec<ReplicationAckRow>, rusqlite::Error> {
        let conn = self.lock_conn()?;
        let mut stmt = conn.prepare(
            "SELECT peer_id, last_event_id, updated_at
             FROM replication_ack
             ORDER BY peer_id ASC",
        )?;
        let rows = stmt.query_map([], |row| {
            Ok(ReplicationAckRow {
                peer_id: row.get(0)?,
                last_event_id: row.get(1)?,
                updated_at: row.get(2)?,
            })
        })?;
        rows.collect()
    }

    pub fn upsert_controller_peer(
        &self,
        controller_id: &str,
        address: &str,
        dc_id: &str,
    ) -> Result<(), rusqlite::Error> {
        let conn = self.lock_conn()?;
        conn.execute(
            "INSERT INTO controller_peers (controller_id, address, dc_id, last_seen_at)
             VALUES (?1, ?2, ?3, datetime('now'))
             ON CONFLICT(controller_id) DO UPDATE SET
                address = excluded.address,
                dc_id = excluded.dc_id,
                last_seen_at = datetime('now')",
            params![controller_id, address, dc_id],
        )?;
        Ok(())
    }

    /// Like `upsert_controller_peer` but only updates `address` and
    /// `last_seen_at`, leaving `dc_id` unchanged. Used by the ack handler
    /// so that a receiver doesn't clobber the peer's real DC identity
    /// (which is set authoritatively by `controller.register` materialization).
    pub fn upsert_controller_peer_address_only(
        &self,
        controller_id: &str,
        address: &str,
    ) -> Result<(), rusqlite::Error> {
        let conn = self.lock_conn()?;
        conn.execute(
            "INSERT INTO controller_peers (controller_id, address, dc_id, last_seen_at)
             VALUES (?1, ?2, 'DC1', datetime('now'))
             ON CONFLICT(controller_id) DO UPDATE SET
                address = excluded.address,
                last_seen_at = datetime('now')",
            params![controller_id, address],
        )?;
        Ok(())
    }

    pub fn list_controller_peers(&self) -> Result<Vec<ControllerPeerRow>, rusqlite::Error> {
        let conn = self.lock_conn()?;
        let mut stmt = conn.prepare(
            "SELECT controller_id, address, dc_id, last_seen_at
             FROM controller_peers
             ORDER BY controller_id ASC",
        )?;
        let rows = stmt.query_map([], |row| {
            Ok(ControllerPeerRow {
                controller_id: row.get(0)?,
                address: row.get(1)?,
                dc_id: row.get(2)?,
                last_seen_at: row.get(3)?,
            })
        })?;
        rows.collect()
    }

    pub fn replication_received_op_exists(&self, op_id: &str) -> Result<bool, rusqlite::Error> {
        let conn = self.lock_conn()?;
        let count: i64 = conn.query_row(
            "SELECT COUNT(*) FROM replication_received_ops WHERE op_id = ?1",
            params![op_id],
            |row| row.get(0),
        )?;
        Ok(count > 0)
    }

    pub fn insert_replication_received_op(
        &self,
        op_id: &str,
        origin_controller_id: &str,
        event_type: &str,
        resource_key: &str,
    ) -> Result<(), rusqlite::Error> {
        let conn = self.lock_conn()?;
        conn.execute(
            "INSERT INTO replication_received_ops (op_id, origin_controller_id, event_type, resource_key)
             VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT(op_id) DO NOTHING",
            params![op_id, origin_controller_id, event_type, resource_key],
        )?;
        Ok(())
    }

    pub fn get_replication_resource_head(
        &self,
        resource_key: &str,
    ) -> Result<Option<ReplicationResourceHeadRow>, rusqlite::Error> {
        let conn = self.lock_conn()?;
        let mut stmt = conn.prepare(
            "SELECT resource_key, last_op_id, last_logical_ts_unix_ms,
                    last_policy_priority, last_intent_epoch, last_validity, last_safety_class,
                    last_controller_id, last_event_id, last_event_type, last_body_json
             FROM replication_resource_heads
             WHERE resource_key = ?1",
        )?;
        let mut rows = stmt.query_map(params![resource_key], |row| {
            Ok(ReplicationResourceHeadRow {
                resource_key: row.get(0)?,
                last_op_id: row.get(1)?,
                last_logical_ts_unix_ms: row.get(2)?,
                last_policy_priority: row.get(3)?,
                last_intent_epoch: row.get(4)?,
                last_validity: row.get(5)?,
                last_safety_class: row.get(6)?,
                last_controller_id: row.get(7)?,
                last_event_id: row.get(8)?,
                last_event_type: row.get(9)?,
                last_body_json: row.get(10)?,
            })
        })?;
        rows.next().transpose()
    }

    pub fn upsert_replication_resource_head(
        &self,
        row: &ReplicationResourceHeadRow,
    ) -> Result<(), rusqlite::Error> {
        let conn = self.lock_conn()?;
        conn.execute(
            "INSERT INTO replication_resource_heads (
                resource_key, last_op_id, last_logical_ts_unix_ms,
                last_policy_priority, last_intent_epoch, last_validity, last_safety_class,
                last_controller_id,
                last_event_id, last_event_type, last_body_json, updated_at
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, datetime('now'))
             ON CONFLICT(resource_key) DO UPDATE SET
                last_op_id=excluded.last_op_id,
                last_logical_ts_unix_ms=excluded.last_logical_ts_unix_ms,
                last_policy_priority=excluded.last_policy_priority,
                last_intent_epoch=excluded.last_intent_epoch,
                last_validity=excluded.last_validity,
                last_safety_class=excluded.last_safety_class,
                last_controller_id=excluded.last_controller_id,
                last_event_id=excluded.last_event_id,
                last_event_type=excluded.last_event_type,
                last_body_json=excluded.last_body_json,
                updated_at=datetime('now')",
            params![
                &row.resource_key,
                &row.last_op_id,
                row.last_logical_ts_unix_ms,
                row.last_policy_priority,
                row.last_intent_epoch,
                &row.last_validity,
                &row.last_safety_class,
                &row.last_controller_id,
                row.last_event_id,
                &row.last_event_type,
                &row.last_body_json,
            ],
        )?;
        Ok(())
    }

    pub fn insert_replication_conflict(
        &self,
        resource_key: &str,
        incumbent_op_id: &str,
        challenger_op_id: &str,
        incumbent_controller_id: &str,
        challenger_controller_id: &str,
        reason: &str,
    ) -> Result<i64, rusqlite::Error> {
        self.insert_replication_conflict_with_resolved(
            resource_key,
            incumbent_op_id,
            challenger_op_id,
            incumbent_controller_id,
            challenger_controller_id,
            reason,
            false,
        )
    }

    pub fn insert_replication_conflict_with_resolved(
        &self,
        resource_key: &str,
        incumbent_op_id: &str,
        challenger_op_id: &str,
        incumbent_controller_id: &str,
        challenger_controller_id: &str,
        reason: &str,
        resolved: bool,
    ) -> Result<i64, rusqlite::Error> {
        let conn = self.lock_conn()?;
        conn.execute(
            "INSERT INTO replication_conflicts (
                resource_key, incumbent_op_id, challenger_op_id,
                incumbent_controller_id, challenger_controller_id, reason, resolved
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![
                resource_key,
                incumbent_op_id,
                challenger_op_id,
                incumbent_controller_id,
                challenger_controller_id,
                reason,
                if resolved { 1 } else { 0 }
            ],
        )?;
        Ok(conn.last_insert_rowid())
    }

    pub fn count_unresolved_replication_conflicts(&self) -> Result<i64, rusqlite::Error> {
        let conn = self.lock_conn()?;
        conn.query_row(
            "SELECT COUNT(*) FROM replication_conflicts WHERE resolved = 0",
            [],
            |row| row.get(0),
        )
    }

    pub fn list_unresolved_replication_conflicts(
        &self,
        limit: i64,
    ) -> Result<Vec<ReplicationConflictRow>, rusqlite::Error> {
        let conn = self.lock_conn()?;
        let mut stmt = conn.prepare(
            "SELECT id, resource_key, incumbent_op_id, challenger_op_id,
                    incumbent_controller_id, challenger_controller_id, reason
             FROM replication_conflicts
             WHERE resolved = 0
             ORDER BY id DESC
             LIMIT ?1",
        )?;
        let rows = stmt.query_map(params![limit], |row| {
            Ok(ReplicationConflictRow {
                id: row.get(0)?,
                resource_key: row.get(1)?,
                incumbent_op_id: row.get(2)?,
                challenger_op_id: row.get(3)?,
                incumbent_controller_id: row.get(4)?,
                challenger_controller_id: row.get(5)?,
                reason: row.get(6)?,
            })
        })?;
        rows.collect()
    }

    pub fn resolve_replication_conflict(&self, id: i64) -> Result<bool, rusqlite::Error> {
        let conn = self.lock_conn()?;
        let rows = conn.execute(
            "UPDATE replication_conflicts
             SET resolved = 1
             WHERE id = ?1 AND resolved = 0",
            params![id],
        )?;
        Ok(rows > 0)
    }

    pub fn insert_compensation_job(
        &self,
        conflict_id: i64,
        resource_key: &str,
        loser_op_id: &str,
        loser_event_type: &str,
        loser_body_json: &str,
    ) -> Result<i64, rusqlite::Error> {
        let conn = self.lock_conn()?;
        conn.execute(
            "INSERT INTO replication_compensation_jobs (conflict_id, resource_key, loser_op_id, loser_event_type, loser_body_json)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![
                conflict_id,
                resource_key,
                loser_op_id,
                loser_event_type,
                loser_body_json
            ],
        )?;
        Ok(conn.last_insert_rowid())
    }

    pub fn count_pending_compensation_jobs(&self) -> Result<i64, rusqlite::Error> {
        let conn = self.lock_conn()?;
        conn.query_row(
            "SELECT COUNT(*) FROM replication_compensation_jobs WHERE status = 'pending'",
            [],
            |row| row.get(0),
        )
    }

    pub fn count_failed_compensation_jobs(&self) -> Result<i64, rusqlite::Error> {
        let conn = self.lock_conn()?;
        conn.query_row(
            "SELECT COUNT(*) FROM replication_compensation_jobs WHERE status = 'failed'",
            [],
            |row| row.get(0),
        )
    }

    pub fn claim_next_compensation_job(
        &self,
    ) -> Result<Option<ReplicationCompensationJobRow>, rusqlite::Error> {
        let conn = self.lock_conn()?;
        let mut stmt = conn.prepare(
            "SELECT id, conflict_id, resource_key, loser_op_id, loser_event_type, loser_body_json, status, attempts
             FROM replication_compensation_jobs
             WHERE status IN ('pending', 'failed')
             ORDER BY id ASC
             LIMIT 1",
        )?;
        let mut rows = stmt.query_map([], |row| {
            Ok(ReplicationCompensationJobRow {
                id: row.get(0)?,
                conflict_id: row.get(1)?,
                resource_key: row.get(2)?,
                loser_op_id: row.get(3)?,
                loser_event_type: row.get(4)?,
                loser_body_json: row.get(5)?,
                status: row.get(6)?,
                attempts: row.get(7)?,
            })
        })?;
        let Some(job) = rows.next().transpose()? else {
            return Ok(None);
        };
        let updated = conn.execute(
            "UPDATE replication_compensation_jobs
             SET status = 'running', attempts = attempts + 1, updated_at = datetime('now')
             WHERE id = ?1 AND status IN ('pending', 'failed')",
            params![job.id],
        )?;
        if updated == 0 {
            return Ok(None);
        }
        let mut claimed = job;
        claimed.status = "running".to_string();
        claimed.attempts += 1;
        Ok(Some(claimed))
    }

    pub fn complete_compensation_job(&self, id: i64) -> Result<(), rusqlite::Error> {
        let conn = self.lock_conn()?;
        conn.execute(
            "UPDATE replication_compensation_jobs
             SET status = 'completed', updated_at = datetime('now')
             WHERE id = ?1",
            params![id],
        )?;
        Ok(())
    }

    pub fn fail_compensation_job(&self, id: i64, error: &str) -> Result<(), rusqlite::Error> {
        let conn = self.lock_conn()?;
        conn.execute(
            "UPDATE replication_compensation_jobs
             SET status = 'failed', last_error = ?2, updated_at = datetime('now')
             WHERE id = ?1",
            params![id, error],
        )?;
        Ok(())
    }

    pub fn get_compensation_job_status_for_loser_op(
        &self,
        loser_op_id: &str,
    ) -> Result<Option<String>, rusqlite::Error> {
        let conn = self.lock_conn()?;
        let mut stmt = conn.prepare(
            "SELECT status
             FROM replication_compensation_jobs
             WHERE loser_op_id = ?1
             ORDER BY id DESC
             LIMIT 1",
        )?;
        let mut rows = stmt.query_map(params![loser_op_id], |row| row.get::<_, String>(0))?;
        rows.next().transpose()
    }

    pub fn list_replication_resource_heads(
        &self,
        limit: i64,
    ) -> Result<Vec<ReplicationResourceHeadRow>, rusqlite::Error> {
        let conn = self.lock_conn()?;
        let mut stmt = conn.prepare(
            "SELECT resource_key, last_op_id, last_logical_ts_unix_ms,
                    last_policy_priority, last_intent_epoch, last_validity, last_safety_class,
                    last_controller_id, last_event_id, last_event_type, last_body_json
             FROM replication_resource_heads
             ORDER BY last_event_id ASC
             LIMIT ?1",
        )?;
        let rows = stmt.query_map(params![limit], |row| {
            Ok(ReplicationResourceHeadRow {
                resource_key: row.get(0)?,
                last_op_id: row.get(1)?,
                last_logical_ts_unix_ms: row.get(2)?,
                last_policy_priority: row.get(3)?,
                last_intent_epoch: row.get(4)?,
                last_validity: row.get(5)?,
                last_safety_class: row.get(6)?,
                last_controller_id: row.get(7)?,
                last_event_id: row.get(8)?,
                last_event_type: row.get(9)?,
                last_body_json: row.get(10)?,
            })
        })?;
        rows.collect()
    }

    pub fn get_materialized_replication_head(
        &self,
        resource_key: &str,
    ) -> Result<Option<ReplicationMaterializedHeadRow>, rusqlite::Error> {
        let conn = self.lock_conn()?;
        let mut stmt = conn.prepare(
            "SELECT resource_key, last_op_id, last_event_type
             FROM replication_materialized_heads
             WHERE resource_key = ?1",
        )?;
        let mut rows = stmt.query_map(params![resource_key], |row| {
            Ok(ReplicationMaterializedHeadRow {
                resource_key: row.get(0)?,
                last_op_id: row.get(1)?,
                last_event_type: row.get(2)?,
            })
        })?;
        rows.next().transpose()
    }

    pub fn upsert_materialized_replication_head(
        &self,
        resource_key: &str,
        last_op_id: &str,
        last_event_type: &str,
    ) -> Result<(), rusqlite::Error> {
        let conn = self.lock_conn()?;
        conn.execute(
            "INSERT INTO replication_materialized_heads (resource_key, last_op_id, last_event_type, updated_at)
             VALUES (?1, ?2, ?3, datetime('now'))
             ON CONFLICT(resource_key) DO UPDATE SET
                last_op_id=excluded.last_op_id,
                last_event_type=excluded.last_event_type,
                updated_at=datetime('now')",
            params![resource_key, last_op_id, last_event_type],
        )?;
        Ok(())
    }

    pub fn upsert_replication_reservation(
        &self,
        reservation_key: &str,
        resource_key: &str,
        op_id: &str,
        status: &str,
        error: &str,
    ) -> Result<(), rusqlite::Error> {
        self.upsert_replication_reservation_with_retry(
            reservation_key,
            resource_key,
            op_id,
            status,
            error,
            0,
        )
    }

    pub fn upsert_replication_reservation_with_retry(
        &self,
        reservation_key: &str,
        resource_key: &str,
        op_id: &str,
        status: &str,
        error: &str,
        retry_count: i32,
    ) -> Result<(), rusqlite::Error> {
        let conn = self.lock_conn()?;
        conn.execute(
            "INSERT INTO replication_reservations (
                reservation_key, resource_key, op_id, status, error, retry_count, updated_at
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, datetime('now'))
             ON CONFLICT(reservation_key, resource_key) DO UPDATE SET
                op_id=excluded.op_id,
                status=excluded.status,
                error=excluded.error,
                retry_count=excluded.retry_count,
                updated_at=datetime('now')",
            params![
                reservation_key,
                resource_key,
                op_id,
                status,
                error,
                retry_count
            ],
        )?;
        Ok(())
    }

    pub fn record_replication_reservation_failure(
        &self,
        reservation_key: &str,
        resource_key: &str,
        op_id: &str,
        retryable: bool,
        error: &str,
        max_retries: i32,
    ) -> Result<(String, i32), rusqlite::Error> {
        let current_retry = self
            .get_replication_reservation(reservation_key, resource_key)?
            .map(|r| r.retry_count)
            .unwrap_or(0);
        let next_retry = current_retry + 1;
        let status = if retryable {
            if next_retry >= max_retries {
                "retry_exhausted"
            } else {
                "failed_retryable"
            }
        } else {
            "failed_non_retryable"
        };
        self.upsert_replication_reservation_with_retry(
            reservation_key,
            resource_key,
            op_id,
            status,
            error,
            next_retry,
        )?;
        Ok((status.to_string(), next_retry))
    }

    pub fn count_failed_replication_reservations(&self) -> Result<i64, rusqlite::Error> {
        let conn = self.lock_conn()?;
        conn.query_row(
            "SELECT COUNT(*) FROM replication_reservations
             WHERE status IN ('failed_retryable', 'failed_non_retryable', 'retry_exhausted')",
            [],
            |row| row.get(0),
        )
    }

    pub fn count_failed_retryable_replication_reservations(&self) -> Result<i64, rusqlite::Error> {
        let conn = self.lock_conn()?;
        conn.query_row(
            "SELECT COUNT(*) FROM replication_reservations WHERE status = 'failed_retryable'",
            [],
            |row| row.get(0),
        )
    }

    pub fn count_failed_non_retryable_replication_reservations(
        &self,
    ) -> Result<i64, rusqlite::Error> {
        let conn = self.lock_conn()?;
        conn.query_row(
            "SELECT COUNT(*) FROM replication_reservations WHERE status = 'failed_non_retryable'",
            [],
            |row| row.get(0),
        )
    }

    pub fn count_retry_exhausted_replication_reservations(&self) -> Result<i64, rusqlite::Error> {
        let conn = self.lock_conn()?;
        conn.query_row(
            "SELECT COUNT(*) FROM replication_reservations WHERE status = 'retry_exhausted'",
            [],
            |row| row.get(0),
        )
    }

    pub fn list_retryable_replication_reservations(
        &self,
        limit: i64,
        min_age_seconds: i64,
    ) -> Result<Vec<ReplicationReservationRow>, rusqlite::Error> {
        let conn = self.lock_conn()?;
        let mut stmt = conn.prepare(
            "SELECT reservation_key, resource_key, op_id, status, error, retry_count
             FROM replication_reservations
             WHERE status = 'failed_retryable'
               AND ((julianday('now') - julianday(updated_at)) * 86400) >= ?1
             ORDER BY updated_at ASC
             LIMIT ?2",
        )?;
        let rows = stmt.query_map(params![min_age_seconds, limit], |row| {
            Ok(ReplicationReservationRow {
                reservation_key: row.get(0)?,
                resource_key: row.get(1)?,
                op_id: row.get(2)?,
                status: row.get(3)?,
                error: row.get(4)?,
                retry_count: row.get(5)?,
            })
        })?;
        rows.collect()
    }

    pub fn get_replication_reservation(
        &self,
        reservation_key: &str,
        resource_key: &str,
    ) -> Result<Option<ReplicationReservationRow>, rusqlite::Error> {
        let conn = self.lock_conn()?;
        let mut stmt = conn.prepare(
            "SELECT reservation_key, resource_key, op_id, status, error, retry_count
             FROM replication_reservations
             WHERE reservation_key = ?1 AND resource_key = ?2",
        )?;
        let mut rows = stmt.query_map(params![reservation_key, resource_key], |row| {
            Ok(ReplicationReservationRow {
                reservation_key: row.get(0)?,
                resource_key: row.get(1)?,
                op_id: row.get(2)?,
                status: row.get(3)?,
                error: row.get(4)?,
                retry_count: row.get(5)?,
            })
        })?;
        rows.next().transpose()
    }

    pub fn count_replication_materialization_backlog(&self) -> Result<i64, rusqlite::Error> {
        let conn = self.lock_conn()?;
        conn.query_row(
            "SELECT COUNT(*)
             FROM replication_resource_heads h
             LEFT JOIN replication_materialized_heads m
               ON m.resource_key = h.resource_key
             WHERE m.last_op_id IS NULL OR m.last_op_id != h.last_op_id",
            [],
            |row| row.get(0),
        )
    }

    pub fn oldest_unresolved_conflict_age_seconds(&self) -> Result<i64, rusqlite::Error> {
        let conn = self.lock_conn()?;
        conn.query_row(
            "SELECT COALESCE(CAST(MAX((julianday('now') - julianday(created_at)) * 86400) AS INTEGER), 0)
             FROM replication_conflicts
             WHERE resolved = 0",
            [],
            |row| row.get(0),
        )
    }

    pub fn upsert_node(&self, node: &NodeRow) -> Result<(), rusqlite::Error> {
        let conn = self.lock_conn()?;
        conn.execute(
            "INSERT INTO nodes (id, hostname, address, cpu_cores, memory_bytes, status, last_heartbeat, gateway_interface, cpu_used, memory_used, storage_backend, disable_vxlan, approval_status, cert_expiry_days, luks_method, dc_id)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16)
             ON CONFLICT(id) DO UPDATE SET
                hostname=excluded.hostname,
                address=excluded.address,
                cpu_cores=excluded.cpu_cores,
                memory_bytes=excluded.memory_bytes,
                status=excluded.status,
                last_heartbeat=excluded.last_heartbeat,
                gateway_interface=excluded.gateway_interface,
                cpu_used=excluded.cpu_used,
                memory_used=excluded.memory_used,
                storage_backend=excluded.storage_backend,
                disable_vxlan=excluded.disable_vxlan,
                cert_expiry_days=excluded.cert_expiry_days,
                luks_method=excluded.luks_method,
                dc_id=excluded.dc_id",
            params![
                node.id,
                node.hostname,
                node.address,
                node.cpu_cores,
                node.memory_bytes,
                node.status,
                node.last_heartbeat,
                node.gateway_interface,
                node.cpu_used,
                node.memory_used,
                node.storage_backend,
                node.disable_vxlan as i32,
                node.approval_status,
                node.cert_expiry_days,
                node.luks_method,
                node.dc_id,
            ],
        )?;
        Ok(())
    }

    pub fn set_node_approval(&self, node_id: &str, status: &str) -> Result<bool, rusqlite::Error> {
        let conn = self.lock_conn()?;
        let rows = conn.execute(
            "UPDATE nodes SET approval_status = ?2 WHERE id = ?1",
            params![node_id, status],
        )?;
        Ok(rows > 0)
    }

    pub fn update_heartbeat(
        &self,
        node_id: &str,
        cpu_used: i32,
        mem_used: i64,
        cert_expiry_days: i32,
        luks_method: &str,
    ) -> Result<bool, rusqlite::Error> {
        let conn = self.lock_conn()?;
        let rows = conn.execute(
            "UPDATE nodes SET last_heartbeat = datetime('now'), \
             status = CASE \
               WHEN status IN ('cordoned', 'draining', 'drained') THEN status \
               WHEN approval_status = 'approved' THEN 'ready' \
               ELSE status \
             END, \
             cpu_used = ?2, memory_used = ?3, cert_expiry_days = ?4, luks_method = ?5 \
             WHERE id = ?1",
            params![node_id, cpu_used, mem_used, cert_expiry_days, luks_method],
        )?;
        Ok(rows > 0)
    }

    pub fn get_node(&self, node_id: &str) -> Result<Option<NodeRow>, rusqlite::Error> {
        let conn = self.lock_conn()?;
        let mut stmt = conn.prepare(
            "SELECT id, hostname, address, cpu_cores, memory_bytes, status, last_heartbeat, gateway_interface, cpu_used, memory_used, storage_backend, disable_vxlan, approval_status, cert_expiry_days, luks_method, dc_id FROM nodes WHERE id = ?1",
        )?;
        let mut rows = stmt.query_map(params![node_id], row_to_node)?;
        rows.next().transpose()
    }

    pub fn list_nodes(&self) -> Result<Vec<NodeRow>, rusqlite::Error> {
        let conn = self.lock_conn()?;
        let mut stmt = conn.prepare(
            "SELECT id, hostname, address, cpu_cores, memory_bytes, status, last_heartbeat, gateway_interface, cpu_used, memory_used, storage_backend, disable_vxlan, approval_status, cert_expiry_days, luks_method, dc_id FROM nodes",
        )?;
        let rows = stmt.query_map([], row_to_node)?;
        rows.collect()
    }

    pub fn get_node_by_address(&self, address: &str) -> Result<Option<NodeRow>, rusqlite::Error> {
        let conn = self.lock_conn()?;
        let mut stmt = conn.prepare(
            "SELECT id, hostname, address, cpu_cores, memory_bytes, status, last_heartbeat, gateway_interface, cpu_used, memory_used, storage_backend, disable_vxlan, approval_status, cert_expiry_days, luks_method, dc_id FROM nodes WHERE address = ?1",
        )?;
        let mut rows = stmt.query_map(params![address], row_to_node)?;
        rows.next().transpose()
    }

    pub fn insert_vm(&self, vm: &VmRow) -> Result<(), rusqlite::Error> {
        let conn = self.lock_conn()?;
        conn.execute(
            "INSERT INTO vms (id, name, cpu, memory_bytes, image_path, image_url, image_sha256, image_format, image_size, network, auto_start, node_id, created_at, runtime_state, cloud_init_user_data, storage_backend, storage_size_bytes, vm_ip, pci_devices)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, datetime('now'), ?13, ?14, ?15, ?16, ?17, ?18)",
            params![
                vm.id,
                vm.name,
                vm.cpu,
                vm.memory_bytes,
                vm.image_path,
                vm.image_url,
                vm.image_sha256,
                vm.image_format,
                vm.image_size,
                vm.network,
                vm.auto_start as i32,
                vm.node_id,
                vm.runtime_state,
                vm.cloud_init_user_data,
                vm.storage_backend,
                vm.storage_size_bytes,
                vm.vm_ip,
                vm.pci_devices,
            ],
        )?;
        Ok(())
    }

    pub fn replace_vm_nics(&self, vm_id: &str, nics: &[VmNicRow]) -> Result<(), rusqlite::Error> {
        let conn = self.lock_conn()?;
        conn.execute("DELETE FROM vm_nics WHERE vm_id = ?1", params![vm_id])?;
        for nic in nics {
            conn.execute(
                "INSERT INTO vm_nics (vm_id, position, network, mac_address, model, ip_address)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                params![
                    vm_id,
                    nic.position,
                    nic.network,
                    nic.mac_address,
                    nic.model,
                    nic.ip_address,
                ],
            )?;
        }
        Ok(())
    }

    pub fn insert_vm_nic(&self, nic: &VmNicRow) -> Result<(), rusqlite::Error> {
        let conn = self.lock_conn()?;
        conn.execute(
            "INSERT INTO vm_nics (vm_id, position, network, mac_address, model, ip_address)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                nic.vm_id,
                nic.position,
                nic.network,
                nic.mac_address,
                nic.model,
                nic.ip_address,
            ],
        )?;
        Ok(())
    }

    pub fn list_vm_nics(&self, vm_id: &str) -> Result<Vec<VmNicRow>, rusqlite::Error> {
        let conn = self.lock_conn()?;
        let mut stmt = conn.prepare(
            "SELECT vm_id, position, network, mac_address, model, ip_address
             FROM vm_nics WHERE vm_id = ?1 ORDER BY position",
        )?;
        let rows = stmt.query_map(params![vm_id], |row| {
            Ok(VmNicRow {
                vm_id: row.get(0)?,
                position: row.get(1)?,
                network: row.get(2)?,
                mac_address: row.get(3)?,
                model: row.get(4)?,
                ip_address: row.get(5)?,
            })
        })?;
        rows.collect()
    }

    pub fn list_all_vm_nics(&self) -> Result<Vec<VmNicRow>, rusqlite::Error> {
        let conn = self.lock_conn()?;
        let mut stmt = conn.prepare(
            "SELECT vm_id, position, network, mac_address, model, ip_address
             FROM vm_nics ORDER BY vm_id, position",
        )?;
        let rows = stmt.query_map([], |row| {
            Ok(VmNicRow {
                vm_id: row.get(0)?,
                position: row.get(1)?,
                network: row.get(2)?,
                mac_address: row.get(3)?,
                model: row.get(4)?,
                ip_address: row.get(5)?,
            })
        })?;
        rows.collect()
    }

    pub fn list_nic_ips_on_node_network(
        &self,
        node_id: &str,
        network: &str,
    ) -> Result<Vec<String>, rusqlite::Error> {
        let conn = self.lock_conn()?;
        let mut stmt = conn.prepare(
            "SELECT n.ip_address FROM vm_nics n
             JOIN vms v ON v.id = n.vm_id
             WHERE v.node_id = ?1 AND n.network = ?2 AND n.ip_address != ''",
        )?;
        let rows = stmt.query_map(params![node_id, network], |row| row.get::<_, String>(0))?;
        rows.collect()
    }

    pub fn set_vm_anti_affinity(
        &self,
        vm_id: &str,
        anti_affinity: &str,
    ) -> Result<(), rusqlite::Error> {
        let conn = self.lock_conn()?;
        conn.execute(
            "INSERT INTO vm_placement (vm_id, anti_affinity) VALUES (?1, ?2)
             ON CONFLICT(vm_id) DO UPDATE SET anti_affinity = excluded.anti_affinity",
            params![vm_id, anti_affinity],
        )?;
        Ok(())
    }

    pub fn get_vm_anti_affinity(&self, vm_id: &str) -> Result<String, rusqlite::Error> {
        let conn = self.lock_conn()?;
        match conn.query_row(
            "SELECT anti_affinity FROM vm_placement WHERE vm_id = ?1",
            params![vm_id],
            |row| row.get::<_, String>(0),
        ) {
            Ok(value) => Ok(value),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(String::new()),
            Err(err) => Err(err),
        }
    }

    pub fn anti_affinity_groups_by_node(
        &self,
    ) -> Result<std::collections::HashMap<String, Vec<String>>, rusqlite::Error> {
        let conn = self.lock_conn()?;
        let mut stmt = conn.prepare(
            "SELECT v.node_id, p.anti_affinity
             FROM vm_placement p
             JOIN vms v ON v.id = p.vm_id
             WHERE p.anti_affinity != ''",
        )?;
        let rows = stmt.query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })?;
        let mut out: std::collections::HashMap<String, Vec<String>> =
            std::collections::HashMap::new();
        for row in rows {
            let (node_id, group) = row?;
            out.entry(node_id).or_default().push(group);
        }
        Ok(out)
    }

    /// Insert a VM row, or update it in place when the id already exists.
    ///
    /// Replication materialization replays `vm.create` for VMs a peer may
    /// already hold. Emulating that with delete-then-insert would cascade the
    /// VM's `vm_ssh_keys` and `security_group_vm_attachments` rows away, so the
    /// conflict is resolved with an `UPDATE` instead. `created_at` is left
    /// alone: the VM is not being recreated.
    pub fn upsert_vm(&self, vm: &VmRow) -> Result<(), rusqlite::Error> {
        let conn = self.lock_conn()?;
        conn.execute(
            "INSERT INTO vms (id, name, cpu, memory_bytes, image_path, image_url, image_sha256, image_format, image_size, network, auto_start, node_id, created_at, runtime_state, cloud_init_user_data, storage_backend, storage_size_bytes, vm_ip, pci_devices)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, datetime('now'), ?13, ?14, ?15, ?16, ?17, ?18)
             ON CONFLICT(id) DO UPDATE SET
                name=excluded.name,
                cpu=excluded.cpu,
                memory_bytes=excluded.memory_bytes,
                image_path=excluded.image_path,
                image_url=excluded.image_url,
                image_sha256=excluded.image_sha256,
                image_format=excluded.image_format,
                image_size=excluded.image_size,
                network=excluded.network,
                auto_start=excluded.auto_start,
                node_id=excluded.node_id,
                runtime_state=excluded.runtime_state,
                cloud_init_user_data=excluded.cloud_init_user_data,
                storage_backend=excluded.storage_backend,
                storage_size_bytes=excluded.storage_size_bytes,
                vm_ip=excluded.vm_ip,
                pci_devices=excluded.pci_devices",
            params![
                vm.id,
                vm.name,
                vm.cpu,
                vm.memory_bytes,
                vm.image_path,
                vm.image_url,
                vm.image_sha256,
                vm.image_format,
                vm.image_size,
                vm.network,
                vm.auto_start as i32,
                vm.node_id,
                vm.runtime_state,
                vm.cloud_init_user_data,
                vm.storage_backend,
                vm.storage_size_bytes,
                vm.vm_ip,
                vm.pci_devices,
            ],
        )?;
        Ok(())
    }

    pub fn upsert_workload(&self, workload: &WorkloadRow) -> Result<(), rusqlite::Error> {
        let conn = self.lock_conn()?;
        conn.execute(
            "INSERT INTO workloads (
                id, name, kind, node_id, runtime_state, desired_state, vm_id, container_image,
                network, storage_backend, storage_size_bytes, created_at
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, datetime('now'))
             ON CONFLICT(id) DO UPDATE SET
                name=excluded.name,
                kind=excluded.kind,
                node_id=excluded.node_id,
                runtime_state=excluded.runtime_state,
                desired_state=excluded.desired_state,
                vm_id=excluded.vm_id,
                container_image=excluded.container_image,
                network=excluded.network,
                storage_backend=excluded.storage_backend,
                storage_size_bytes=excluded.storage_size_bytes",
            params![
                workload.id,
                workload.name,
                workload.kind,
                workload.node_id,
                workload.runtime_state,
                workload.desired_state,
                workload.vm_id,
                workload.container_image,
                workload.network,
                workload.storage_backend,
                workload.storage_size_bytes,
            ],
        )?;
        Ok(())
    }

    pub fn get_workload(
        &self,
        workload_id_or_name: &str,
    ) -> Result<Option<WorkloadRow>, rusqlite::Error> {
        let conn = self.lock_conn()?;
        let mut stmt = conn.prepare(
            "SELECT id, name, kind, node_id, runtime_state, desired_state, vm_id, container_image, network, storage_backend, storage_size_bytes, created_at
             FROM workloads
             WHERE id = ?1 OR name = ?1
             LIMIT 1",
        )?;
        let mut rows = stmt.query_map(params![workload_id_or_name], row_to_workload)?;
        rows.next().transpose()
    }

    pub fn list_workloads(
        &self,
        kind_filter: Option<&str>,
        node_filter: Option<&str>,
    ) -> Result<Vec<WorkloadRow>, rusqlite::Error> {
        let conn = self.lock_conn()?;
        match (kind_filter, node_filter) {
            (Some(kind), Some(node)) => {
                let mut stmt = conn.prepare(
                    "SELECT id, name, kind, node_id, runtime_state, desired_state, vm_id, container_image, network, storage_backend, storage_size_bytes, created_at
                     FROM workloads
                     WHERE kind = ?1 AND node_id = ?2
                     ORDER BY created_at ASC",
                )?;
                let rows = stmt.query_map(params![kind, node], row_to_workload)?;
                rows.collect()
            }
            (Some(kind), None) => {
                let mut stmt = conn.prepare(
                    "SELECT id, name, kind, node_id, runtime_state, desired_state, vm_id, container_image, network, storage_backend, storage_size_bytes, created_at
                     FROM workloads
                     WHERE kind = ?1
                     ORDER BY created_at ASC",
                )?;
                let rows = stmt.query_map(params![kind], row_to_workload)?;
                rows.collect()
            }
            (None, Some(node)) => {
                let mut stmt = conn.prepare(
                    "SELECT id, name, kind, node_id, runtime_state, desired_state, vm_id, container_image, network, storage_backend, storage_size_bytes, created_at
                     FROM workloads
                     WHERE node_id = ?1
                     ORDER BY created_at ASC",
                )?;
                let rows = stmt.query_map(params![node], row_to_workload)?;
                rows.collect()
            }
            (None, None) => {
                let mut stmt = conn.prepare(
                    "SELECT id, name, kind, node_id, runtime_state, desired_state, vm_id, container_image, network, storage_backend, storage_size_bytes, created_at
                     FROM workloads
                     ORDER BY created_at ASC",
                )?;
                let rows = stmt.query_map([], row_to_workload)?;
                rows.collect()
            }
        }
    }

    pub fn update_workload_runtime_state(
        &self,
        id_or_name: &str,
        runtime_state: &str,
    ) -> Result<bool, rusqlite::Error> {
        let conn = self.lock_conn()?;
        let rows = conn.execute(
            "UPDATE workloads
             SET runtime_state = ?2
             WHERE id = ?1 OR name = ?1",
            params![id_or_name, runtime_state],
        )?;
        Ok(rows > 0)
    }

    pub fn update_workload_desired_state(
        &self,
        id_or_name: &str,
        desired_state: &str,
    ) -> Result<bool, rusqlite::Error> {
        let conn = self.lock_conn()?;
        let rows = conn.execute(
            "UPDATE workloads
             SET desired_state = ?2
             WHERE id = ?1 OR name = ?1",
            params![id_or_name, desired_state],
        )?;
        Ok(rows > 0)
    }

    pub fn delete_workload_by_id_or_name(&self, id_or_name: &str) -> Result<bool, rusqlite::Error> {
        let conn = self.lock_conn()?;
        let rows = conn.execute(
            "DELETE FROM workloads WHERE id = ?1 OR name = ?1",
            params![id_or_name],
        )?;
        Ok(rows > 0)
    }

    pub fn get_vm(&self, vm_id: &str) -> Result<Option<VmRow>, rusqlite::Error> {
        let conn = self.lock_conn()?;
        let mut stmt = conn.prepare(
            "SELECT id, name, cpu, memory_bytes, image_path, image_url, image_sha256, image_format, image_size, network, auto_start, node_id, created_at, runtime_state, cloud_init_user_data, storage_backend, storage_size_bytes, vm_ip, pci_devices FROM vms WHERE id = ?1",
        )?;
        let mut rows = stmt.query_map(params![vm_id], row_to_vm)?;
        rows.next().transpose()
    }

    pub fn list_vms(&self) -> Result<Vec<VmRow>, rusqlite::Error> {
        let conn = self.lock_conn()?;
        let mut stmt = conn.prepare(
            "SELECT id, name, cpu, memory_bytes, image_path, image_url, image_sha256, image_format, image_size, network, auto_start, node_id, created_at, runtime_state, cloud_init_user_data, storage_backend, storage_size_bytes, vm_ip, pci_devices FROM vms",
        )?;
        let rows = stmt.query_map([], row_to_vm)?;
        rows.collect()
    }

    pub fn list_vms_for_node(&self, node_id: &str) -> Result<Vec<VmRow>, rusqlite::Error> {
        let conn = self.lock_conn()?;
        let mut stmt = conn.prepare(
            "SELECT id, name, cpu, memory_bytes, image_path, image_url, image_sha256, image_format, image_size, network, auto_start, node_id, created_at, runtime_state, cloud_init_user_data, storage_backend, storage_size_bytes, vm_ip, pci_devices FROM vms WHERE node_id = ?1",
        )?;
        let rows = stmt.query_map(params![node_id], row_to_vm)?;
        rows.collect()
    }

    pub fn insert_network(&self, network: &NetworkRow) -> Result<(), rusqlite::Error> {
        let conn = self.lock_conn()?;
        conn.execute(
            "INSERT INTO networks (name, external_ip, gateway_ip, internal_netmask, node_id, allowed_tcp_ports, allowed_udp_ports, vlan_id, network_type, enable_outbound_nat, vni, next_ip)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
            params![
                network.name,
                network.external_ip,
                network.gateway_ip,
                network.internal_netmask,
                network.node_id,
                network.allowed_tcp_ports,
                network.allowed_udp_ports,
                network.vlan_id,
                network.network_type,
                network.enable_outbound_nat as i32,
                network.vni,
                network.next_ip,
            ],
        )?;
        Ok(())
    }

    pub fn get_network_for_node(
        &self,
        node_id: &str,
        name: &str,
    ) -> Result<Option<NetworkRow>, rusqlite::Error> {
        let conn = self.lock_conn()?;
        let mut stmt = conn.prepare(
            "SELECT name, external_ip, gateway_ip, internal_netmask, node_id, allowed_tcp_ports, allowed_udp_ports, vlan_id, network_type, enable_outbound_nat, vni, next_ip
             FROM networks
             WHERE node_id = ?1 AND name = ?2",
        )?;
        let mut rows = stmt.query_map(params![node_id, name], row_to_network)?;
        rows.next().transpose()
    }

    pub fn list_networks(&self) -> Result<Vec<NetworkRow>, rusqlite::Error> {
        let conn = self.lock_conn()?;
        let mut stmt = conn.prepare(
            "SELECT name, external_ip, gateway_ip, internal_netmask, node_id, allowed_tcp_ports, allowed_udp_ports, vlan_id, network_type, enable_outbound_nat, vni, next_ip
             FROM networks",
        )?;
        let rows = stmt.query_map([], row_to_network)?;
        rows.collect()
    }

    pub fn list_networks_for_node(
        &self,
        node_id: &str,
    ) -> Result<Vec<NetworkRow>, rusqlite::Error> {
        let conn = self.lock_conn()?;
        let mut stmt = conn.prepare(
            "SELECT name, external_ip, gateway_ip, internal_netmask, node_id, allowed_tcp_ports, allowed_udp_ports, vlan_id, network_type, enable_outbound_nat, vni, next_ip
             FROM networks
             WHERE node_id = ?1",
        )?;
        let rows = stmt.query_map(params![node_id], row_to_network)?;
        rows.collect()
    }

    pub fn insert_postgresql(&self, row: &PostgresqlRow) -> Result<(), rusqlite::Error> {
        let conn = self.lock_conn()?;
        conn.execute(
            "INSERT INTO postgresql_instances (name, database_name, package, port, node_id)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![
                row.name,
                row.database_name,
                row.package,
                row.port,
                row.node_id
            ],
        )?;
        Ok(())
    }

    pub fn upsert_postgresql(&self, row: &PostgresqlRow) -> Result<(), rusqlite::Error> {
        let conn = self.lock_conn()?;
        conn.execute(
            "INSERT INTO postgresql_instances (name, database_name, package, port, node_id)
             VALUES (?1, ?2, ?3, ?4, ?5)
             ON CONFLICT(name) DO UPDATE SET
               database_name = excluded.database_name,
               package = excluded.package,
               port = excluded.port,
               node_id = excluded.node_id",
            params![
                row.name,
                row.database_name,
                row.package,
                row.port,
                row.node_id
            ],
        )?;
        Ok(())
    }

    pub fn update_postgresql_port(&self, name: &str, port: i32) -> Result<(), rusqlite::Error> {
        let conn = self.lock_conn()?;
        let changed = conn.execute(
            "UPDATE postgresql_instances SET port = ?1 WHERE name = ?2",
            params![port, name],
        )?;
        if changed == 0 {
            return Err(rusqlite::Error::QueryReturnedNoRows);
        }
        Ok(())
    }

    pub fn get_postgresql(&self, name: &str) -> Result<Option<PostgresqlRow>, rusqlite::Error> {
        let conn = self.lock_conn()?;
        let mut stmt = conn.prepare(
            "SELECT name, database_name, package, port, node_id, created_at
             FROM postgresql_instances WHERE name = ?1",
        )?;
        let mut rows = stmt.query_map(params![name], row_to_postgresql)?;
        rows.next().transpose()
    }

    pub fn get_postgresql_for_node(
        &self,
        node_id: &str,
    ) -> Result<Option<PostgresqlRow>, rusqlite::Error> {
        let conn = self.lock_conn()?;
        let mut stmt = conn.prepare(
            "SELECT name, database_name, package, port, node_id, created_at
             FROM postgresql_instances WHERE node_id = ?1",
        )?;
        let mut rows = stmt.query_map(params![node_id], row_to_postgresql)?;
        rows.next().transpose()
    }

    pub fn list_postgresqls(&self) -> Result<Vec<PostgresqlRow>, rusqlite::Error> {
        let conn = self.lock_conn()?;
        let mut stmt = conn.prepare(
            "SELECT name, database_name, package, port, node_id, created_at
             FROM postgresql_instances ORDER BY name",
        )?;
        let rows = stmt.query_map([], row_to_postgresql)?;
        rows.collect()
    }

    pub fn delete_postgresql(&self, name: &str) -> Result<bool, rusqlite::Error> {
        let conn = self.lock_conn()?;
        let changed = conn.execute(
            "DELETE FROM postgresql_instances WHERE name = ?1",
            params![name],
        )?;
        Ok(changed > 0)
    }

    pub fn list_networks_by_name(&self, name: &str) -> Result<Vec<NetworkRow>, rusqlite::Error> {
        let conn = self.lock_conn()?;
        let mut stmt = conn.prepare(
            "SELECT name, external_ip, gateway_ip, internal_netmask, node_id, allowed_tcp_ports, allowed_udp_ports, vlan_id, network_type, enable_outbound_nat, vni, next_ip
             FROM networks
             WHERE name = ?1",
        )?;
        let rows = stmt.query_map(params![name], row_to_network)?;
        rows.collect()
    }

    pub fn allocate_vm_ip(
        &self,
        network_name: &str,
        node_id: &str,
    ) -> Result<String, rusqlite::Error> {
        let conn = self.lock_conn()?;
        let (gateway_ip, next_ip): (String, i32) = conn.query_row(
            "SELECT gateway_ip, next_ip FROM networks WHERE name = ?1 AND node_id = ?2",
            params![network_name, node_id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )?;
        conn.execute(
            "UPDATE networks SET next_ip = next_ip + 1 WHERE name = ?1 AND node_id = ?2",
            params![network_name, node_id],
        )?;
        let prefix = gateway_ip.rsplit_once('.').map(|x| x.0).unwrap_or("10.0.0");
        Ok(format!("{}.{}", prefix, next_ip))
    }

    /// Allocate a VM IP that is unique across *all* nodes sharing this
    /// network name (used for VXLAN overlays where every node is in the
    /// same L2 domain). Picks the global max `next_ip`, returns it, and
    /// bumps every row's counter so the next call is also unique.
    pub fn allocate_vm_ip_global(&self, network_name: &str) -> Result<String, rusqlite::Error> {
        let mut conn = self.lock_conn()?;
        let tx = conn.transaction()?;
        if let Some(ip) = take_released_vxlan_ip(&tx, network_name)? {
            tx.commit()?;
            return Ok(ip);
        }
        let (gateway_ip, global_next): (String, i32) = tx.query_row(
            "SELECT gateway_ip, MAX(next_ip) FROM networks WHERE name = ?1",
            params![network_name],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )?;
        let new_next = global_next + 1;
        tx.execute(
            "UPDATE networks SET next_ip = ?1 WHERE name = ?2",
            params![new_next, network_name],
        )?;
        tx.commit()?;
        let prefix = gateway_ip.rsplit_once('.').map(|x| x.0).unwrap_or("10.0.0");
        Ok(format!("{}.{}", prefix, global_next))
    }

    pub fn delete_network(&self, node_id: &str, name: &str) -> Result<bool, rusqlite::Error> {
        let conn = self.lock_conn()?;
        conn.execute(
            "DELETE FROM network_policy WHERE node_id = ?1 AND name = ?2",
            params![node_id, name],
        )?;
        let rows = conn.execute(
            "DELETE FROM networks WHERE node_id = ?1 AND name = ?2",
            params![node_id, name],
        )?;
        Ok(rows > 0)
    }

    pub fn upsert_network_policy(&self, policy: &NetworkPolicyRow) -> Result<(), rusqlite::Error> {
        let conn = self.lock_conn()?;
        conn.execute(
            "INSERT INTO network_policy (node_id, name, east_west, ipv6_prefix, ipv6_gateway, ipv6_next)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)
             ON CONFLICT(node_id, name) DO UPDATE SET
               east_west = excluded.east_west,
               ipv6_prefix = excluded.ipv6_prefix,
               ipv6_gateway = excluded.ipv6_gateway",
            params![
                policy.node_id,
                policy.name,
                if policy.east_west { 1 } else { 0 },
                policy.ipv6_prefix,
                policy.ipv6_gateway,
                policy.ipv6_next,
            ],
        )?;
        Ok(())
    }

    /// Stored policy, or an absent row when the network has never set one.
    pub fn get_network_policy(
        &self,
        node_id: &str,
        name: &str,
    ) -> Result<NetworkPolicyRow, rusqlite::Error> {
        let conn = self.lock_conn()?;
        match conn.query_row(
            "SELECT node_id, name, east_west, ipv6_prefix, ipv6_gateway, ipv6_next
             FROM network_policy WHERE node_id = ?1 AND name = ?2",
            params![node_id, name],
            row_to_network_policy,
        ) {
            Ok(row) => Ok(row),
            Err(rusqlite::Error::QueryReturnedNoRows) => {
                Ok(NetworkPolicyRow::absent(node_id, name))
            }
            Err(e) => Err(e),
        }
    }

    pub fn list_network_policies_for_node(
        &self,
        node_id: &str,
    ) -> Result<Vec<NetworkPolicyRow>, rusqlite::Error> {
        let conn = self.lock_conn()?;
        let mut stmt = conn.prepare(
            "SELECT node_id, name, east_west, ipv6_prefix, ipv6_gateway, ipv6_next
             FROM network_policy WHERE node_id = ?1",
        )?;
        let rows = stmt.query_map(params![node_id], row_to_network_policy)?;
        rows.collect()
    }

    /// Allocate one IPv6 address and store it on the VM.
    ///
    /// `global` is true for VXLAN, where every node shares the prefix.
    /// A previously released address is reused before `ipv6_next` advances.
    pub fn allocate_vm_ipv6(
        &self,
        node_id: &str,
        network: &str,
        vm_id: &str,
        position: i32,
        global: bool,
    ) -> Result<String, rusqlite::Error> {
        let mut conn = self.lock_conn()?;
        let tx = conn.transaction()?;
        let scope = crate::net_policy::ipv6_scope(global, node_id, network);
        let address = if let Some(released) = take_released_ipv6(&tx, &scope)? {
            released
        } else {
            let (prefix, next) = if global {
                tx.query_row(
                    "SELECT ipv6_prefix, MAX(ipv6_next) FROM network_policy WHERE name = ?1 AND ipv6_prefix != ''",
                    params![network],
                    |row| Ok((row.get::<_, String>(0)?, row.get::<_, i32>(1)?)),
                )?
            } else {
                tx.query_row(
                    "SELECT ipv6_prefix, ipv6_next FROM network_policy WHERE node_id = ?1 AND name = ?2",
                    params![node_id, network],
                    |row| Ok((row.get::<_, String>(0)?, row.get::<_, i32>(1)?)),
                )?
            };
            let host = u32::try_from(next).unwrap_or(0);
            let address = crate::net_policy::ipv6_for_host(&prefix, host).map_err(|e| {
                rusqlite::Error::ToSqlConversionFailure(Box::new(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    e,
                )))
            })?;
            let new_next = next + 1;
            if global {
                tx.execute(
                    "UPDATE network_policy SET ipv6_next = ?1 WHERE name = ?2",
                    params![new_next, network],
                )?;
            } else {
                tx.execute(
                    "UPDATE network_policy SET ipv6_next = ?1 WHERE node_id = ?2 AND name = ?3",
                    params![new_next, node_id, network],
                )?;
            }
            address
        };
        tx.execute(
            "INSERT INTO vm_ipv6 (vm_id, position, address) VALUES (?1, ?2, ?3)",
            params![vm_id, position, address],
        )?;
        tx.commit()?;
        Ok(address)
    }

    pub fn list_vm_ipv6(&self, vm_id: &str) -> Result<Vec<VmIpv6Row>, rusqlite::Error> {
        let conn = self.lock_conn()?;
        let mut stmt = conn.prepare(
            "SELECT vm_id, position, address FROM vm_ipv6 WHERE vm_id = ?1 ORDER BY position",
        )?;
        let rows = stmt.query_map(params![vm_id], row_to_vm_ipv6)?;
        rows.collect()
    }

    pub fn replace_vm_ipv6(&self, vm_id: &str, rows: &[VmIpv6Row]) -> Result<(), rusqlite::Error> {
        let mut conn = self.lock_conn()?;
        let tx = conn.transaction()?;
        tx.execute("DELETE FROM vm_ipv6 WHERE vm_id = ?1", params![vm_id])?;
        for row in rows {
            tx.execute(
                "INSERT INTO vm_ipv6 (vm_id, position, address) VALUES (?1, ?2, ?3)",
                params![vm_id, row.position, row.address],
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    pub fn upsert_security_group(&self, sg: &SecurityGroupRow) -> Result<(), rusqlite::Error> {
        let conn = self.lock_conn()?;
        conn.execute(
            "INSERT INTO security_groups (name, description)
             VALUES (?1, ?2)
             ON CONFLICT(name) DO UPDATE SET description=excluded.description",
            params![sg.name, sg.description],
        )?;
        Ok(())
    }

    pub fn get_security_group(
        &self,
        name: &str,
    ) -> Result<Option<SecurityGroupRow>, rusqlite::Error> {
        let conn = self.lock_conn()?;
        let mut stmt = conn
            .prepare("SELECT name, description, created_at FROM security_groups WHERE name = ?1")?;
        let mut rows = stmt.query_map(params![name], |row| {
            Ok(SecurityGroupRow {
                name: row.get(0)?,
                description: row.get(1)?,
                created_at: row.get(2)?,
            })
        })?;
        rows.next().transpose()
    }

    pub fn list_security_groups(&self) -> Result<Vec<SecurityGroupRow>, rusqlite::Error> {
        let conn = self.lock_conn()?;
        let mut stmt = conn.prepare(
            "SELECT name, description, created_at FROM security_groups ORDER BY name ASC",
        )?;
        let rows = stmt.query_map([], |row| {
            Ok(SecurityGroupRow {
                name: row.get(0)?,
                description: row.get(1)?,
                created_at: row.get(2)?,
            })
        })?;
        rows.collect()
    }

    pub fn delete_security_group(&self, name: &str) -> Result<bool, rusqlite::Error> {
        let conn = self.lock_conn()?;
        let rows = conn.execute("DELETE FROM security_groups WHERE name = ?1", params![name])?;
        Ok(rows > 0)
    }

    pub fn upsert_disk_layout(
        &self,
        layout: &DiskLayoutRow,
    ) -> Result<DiskLayoutRow, rusqlite::Error> {
        let conn = self.lock_conn()?;
        conn.execute(
            "INSERT INTO disk_layouts (name, node_id, generation, layout_nix, evacuate)
             VALUES (?1, ?2, ?3, ?4, ?5)
             ON CONFLICT(name) DO UPDATE SET
               node_id = excluded.node_id,
               generation = excluded.generation,
               layout_nix = excluded.layout_nix,
               evacuate = excluded.evacuate,
               updated_at = datetime('now')",
            params![
                layout.name,
                layout.node_id,
                layout.generation,
                layout.layout_nix,
                layout.evacuate as i32,
            ],
        )?;
        let mut stmt = conn.prepare(
            "SELECT name, node_id, generation, layout_nix, evacuate, created_at, updated_at
             FROM disk_layouts WHERE name = ?1",
        )?;
        let row = stmt.query_row(params![layout.name], |row| {
            Ok(DiskLayoutRow {
                name: row.get(0)?,
                node_id: row.get(1)?,
                generation: row.get(2)?,
                layout_nix: row.get(3)?,
                evacuate: row.get::<_, i32>(4)? != 0,
                created_at: row.get(5)?,
                updated_at: row.get(6)?,
            })
        })?;
        Ok(row)
    }

    pub fn get_disk_layout(&self, name: &str) -> Result<Option<DiskLayoutRow>, rusqlite::Error> {
        let conn = self.lock_conn()?;
        let mut stmt = conn.prepare(
            "SELECT name, node_id, generation, layout_nix, evacuate, created_at, updated_at
             FROM disk_layouts WHERE name = ?1",
        )?;
        let mut rows = stmt.query_map(params![name], |row| {
            Ok(DiskLayoutRow {
                name: row.get(0)?,
                node_id: row.get(1)?,
                generation: row.get(2)?,
                layout_nix: row.get(3)?,
                evacuate: row.get::<_, i32>(4)? != 0,
                created_at: row.get(5)?,
                updated_at: row.get(6)?,
            })
        })?;
        rows.next().transpose()
    }

    pub fn list_disk_layouts(
        &self,
        node_id_filter: Option<&str>,
    ) -> Result<Vec<DiskLayoutRow>, rusqlite::Error> {
        let conn = self.lock_conn()?;
        if let Some(node_id) = node_id_filter {
            let mut stmt = conn.prepare(
                "SELECT name, node_id, generation, layout_nix, evacuate, created_at, updated_at
                 FROM disk_layouts WHERE node_id = ?1 ORDER BY name ASC",
            )?;
            let rows = stmt.query_map(params![node_id], |row| {
                Ok(DiskLayoutRow {
                    name: row.get(0)?,
                    node_id: row.get(1)?,
                    generation: row.get(2)?,
                    layout_nix: row.get(3)?,
                    evacuate: row.get::<_, i32>(4)? != 0,
                    created_at: row.get(5)?,
                    updated_at: row.get(6)?,
                })
            })?;
            rows.collect()
        } else {
            let mut stmt = conn.prepare(
                "SELECT name, node_id, generation, layout_nix, evacuate, created_at, updated_at
                 FROM disk_layouts ORDER BY name ASC",
            )?;
            let rows = stmt.query_map([], |row| {
                Ok(DiskLayoutRow {
                    name: row.get(0)?,
                    node_id: row.get(1)?,
                    generation: row.get(2)?,
                    layout_nix: row.get(3)?,
                    evacuate: row.get::<_, i32>(4)? != 0,
                    created_at: row.get(5)?,
                    updated_at: row.get(6)?,
                })
            })?;
            rows.collect()
        }
    }

    pub fn delete_disk_layout(&self, name: &str) -> Result<bool, rusqlite::Error> {
        let conn = self.lock_conn()?;
        let rows = conn.execute("DELETE FROM disk_layouts WHERE name = ?1", params![name])?;
        Ok(rows > 0)
    }

    pub fn upsert_disk_layout_status(
        &self,
        status: &DiskLayoutStatusRow,
    ) -> Result<(), rusqlite::Error> {
        let conn = self.lock_conn()?;
        conn.execute(
            "INSERT INTO disk_layout_status (
                name, observed_generation, phase, refusal_reason, message, last_transition_at
             ) VALUES (?1, ?2, ?3, ?4, ?5, datetime('now'))
             ON CONFLICT(name) DO UPDATE SET
               observed_generation = excluded.observed_generation,
               phase = excluded.phase,
               refusal_reason = excluded.refusal_reason,
               message = excluded.message,
               last_transition_at = datetime('now')",
            params![
                status.name,
                status.observed_generation,
                status.phase,
                status.refusal_reason,
                status.message,
            ],
        )?;
        Ok(())
    }

    pub fn get_disk_layout_status(
        &self,
        name: &str,
    ) -> Result<Option<DiskLayoutStatusRow>, rusqlite::Error> {
        let conn = self.lock_conn()?;
        let mut stmt = conn.prepare(
            "SELECT name, observed_generation, phase, refusal_reason, message, last_transition_at
             FROM disk_layout_status WHERE name = ?1",
        )?;
        let mut rows = stmt.query_map(params![name], |row| {
            Ok(DiskLayoutStatusRow {
                name: row.get(0)?,
                observed_generation: row.get(1)?,
                phase: row.get(2)?,
                refusal_reason: row.get(3)?,
                message: row.get(4)?,
                last_transition_at: row.get(5)?,
            })
        })?;
        rows.next().transpose()
    }

    pub fn list_disk_layout_statuses(&self) -> Result<Vec<DiskLayoutStatusRow>, rusqlite::Error> {
        let conn = self.lock_conn()?;
        let mut stmt = conn.prepare(
            "SELECT name, observed_generation, phase, refusal_reason, message, last_transition_at
             FROM disk_layout_status",
        )?;
        let rows = stmt.query_map([], |row| {
            Ok(DiskLayoutStatusRow {
                name: row.get(0)?,
                observed_generation: row.get(1)?,
                phase: row.get(2)?,
                refusal_reason: row.get(3)?,
                message: row.get(4)?,
                last_transition_at: row.get(5)?,
            })
        })?;
        rows.collect()
    }

    /// Return disk layouts whose status has `observed_generation < generation`
    /// (or no status row yet). These are the rows the reconciler should push
    /// to the node-agent on the next tick.
    pub fn list_disk_layouts_needing_reconcile(
        &self,
    ) -> Result<Vec<DiskLayoutRow>, rusqlite::Error> {
        let conn = self.lock_conn()?;
        let mut stmt = conn.prepare(
            "SELECT d.name, d.node_id, d.generation, d.layout_nix, d.evacuate, d.created_at, d.updated_at
             FROM disk_layouts d
             LEFT JOIN disk_layout_status s ON s.name = d.name
             WHERE s.name IS NULL OR s.observed_generation < d.generation
             ORDER BY d.name ASC",
        )?;
        let rows = stmt.query_map([], |row| {
            Ok(DiskLayoutRow {
                name: row.get(0)?,
                node_id: row.get(1)?,
                generation: row.get(2)?,
                layout_nix: row.get(3)?,
                evacuate: row.get::<_, i32>(4)? != 0,
                created_at: row.get(5)?,
                updated_at: row.get(6)?,
            })
        })?;
        rows.collect()
    }

    pub fn upsert_ceph_cluster(&self, row: &CephClusterRow) -> Result<(), rusqlite::Error> {
        let conn = self.lock_conn()?;
        conn.execute(
            "INSERT INTO ceph_clusters (name, generation, spec_json, bootstrap_json, created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, COALESCE(NULLIF(?5, ''), datetime('now')), datetime('now'))
             ON CONFLICT(name) DO UPDATE SET generation=excluded.generation,
               spec_json=excluded.spec_json,
               bootstrap_json=CASE
                 WHEN excluded.bootstrap_json != '' THEN excluded.bootstrap_json
                 ELSE ceph_clusters.bootstrap_json
               END,
               updated_at=datetime('now')",
            params![
                row.name,
                row.generation,
                row.spec_json,
                row.bootstrap_json,
                row.created_at
            ],
        )?;
        Ok(())
    }

    pub fn get_ceph_cluster(&self, name: &str) -> Result<Option<CephClusterRow>, rusqlite::Error> {
        let conn = self.lock_conn()?;
        let mut stmt = conn.prepare(
            "SELECT name, generation, spec_json, COALESCE(bootstrap_json, ''), created_at, updated_at
             FROM ceph_clusters WHERE name=?1",
        )?;
        let mut rows = stmt.query_map(params![name], |r| {
            Ok(CephClusterRow {
                name: r.get(0)?,
                generation: r.get(1)?,
                spec_json: r.get(2)?,
                bootstrap_json: r.get(3)?,
                created_at: r.get(4)?,
                updated_at: r.get(5)?,
            })
        })?;
        rows.next().transpose()
    }

    pub fn list_ceph_clusters(&self) -> Result<Vec<CephClusterRow>, rusqlite::Error> {
        let conn = self.lock_conn()?;
        let mut stmt = conn.prepare(
            "SELECT name, generation, spec_json, COALESCE(bootstrap_json, ''), created_at, updated_at
             FROM ceph_clusters ORDER BY name",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok(CephClusterRow {
                name: r.get(0)?,
                generation: r.get(1)?,
                spec_json: r.get(2)?,
                bootstrap_json: r.get(3)?,
                created_at: r.get(4)?,
                updated_at: r.get(5)?,
            })
        })?;
        rows.collect()
    }

    pub fn set_ceph_cluster_bootstrap(
        &self,
        name: &str,
        bootstrap_json: &str,
    ) -> Result<(), rusqlite::Error> {
        let conn = self.lock_conn()?;
        conn.execute(
            "UPDATE ceph_clusters SET bootstrap_json=?2, updated_at=datetime('now') WHERE name=?1",
            params![name, bootstrap_json],
        )?;
        Ok(())
    }

    pub fn delete_ceph_cluster(&self, name: &str) -> Result<bool, rusqlite::Error> {
        let conn = self.lock_conn()?;
        self.clear_retired_ceph_nodes_conn(&conn, name)?;
        Ok(conn.execute("DELETE FROM ceph_clusters WHERE name=?1", params![name])? > 0)
    }

    pub fn add_retired_ceph_node(
        &self,
        cluster: &str,
        node_id: &str,
    ) -> Result<(), rusqlite::Error> {
        let conn = self.lock_conn()?;
        conn.execute(
            "INSERT INTO ceph_retired_nodes (cluster_name, node_id) VALUES (?1, ?2)
             ON CONFLICT(cluster_name, node_id) DO NOTHING",
            params![cluster, node_id],
        )?;
        Ok(())
    }

    pub fn list_retired_ceph_nodes(&self, cluster: &str) -> Result<Vec<String>, rusqlite::Error> {
        let conn = self.lock_conn()?;
        let mut stmt = conn.prepare(
            "SELECT node_id FROM ceph_retired_nodes WHERE cluster_name=?1 ORDER BY node_id",
        )?;
        let rows = stmt.query_map(params![cluster], |r| r.get(0))?;
        rows.collect()
    }

    pub fn clear_retired_ceph_node(
        &self,
        cluster: &str,
        node_id: &str,
    ) -> Result<(), rusqlite::Error> {
        let conn = self.lock_conn()?;
        conn.execute(
            "DELETE FROM ceph_retired_nodes WHERE cluster_name=?1 AND node_id=?2",
            params![cluster, node_id],
        )?;
        Ok(())
    }

    fn clear_retired_ceph_nodes_conn(
        &self,
        conn: &rusqlite::Connection,
        cluster: &str,
    ) -> Result<(), rusqlite::Error> {
        conn.execute(
            "DELETE FROM ceph_retired_nodes WHERE cluster_name=?1",
            params![cluster],
        )?;
        Ok(())
    }

    pub fn upsert_ceph_cluster_status(
        &self,
        row: &CephClusterStatusRow,
    ) -> Result<(), rusqlite::Error> {
        let conn = self.lock_conn()?;
        conn.execute(
            "INSERT INTO ceph_cluster_status
             (name, observed_generation, phase, health_message, ceph_status_json, last_transition_at)
             VALUES (?1, ?2, ?3, ?4, ?5, datetime('now'))
             ON CONFLICT(name) DO UPDATE SET observed_generation=excluded.observed_generation,
               phase=excluded.phase, health_message=excluded.health_message,
               ceph_status_json=excluded.ceph_status_json, last_transition_at=datetime('now')",
            params![row.name, row.observed_generation, row.phase, row.health_message, row.ceph_status_json],
        )?;
        Ok(())
    }

    pub fn get_ceph_cluster_status(
        &self,
        name: &str,
    ) -> Result<Option<CephClusterStatusRow>, rusqlite::Error> {
        let conn = self.lock_conn()?;
        let mut stmt = conn.prepare(
            "SELECT name, observed_generation, phase, health_message, ceph_status_json,
                    last_transition_at FROM ceph_cluster_status WHERE name=?1",
        )?;
        let mut rows = stmt.query_map(params![name], |r| {
            Ok(CephClusterStatusRow {
                name: r.get(0)?,
                observed_generation: r.get(1)?,
                phase: r.get(2)?,
                health_message: r.get(3)?,
                ceph_status_json: r.get(4)?,
                last_transition_at: r.get(5)?,
            })
        })?;
        rows.next().transpose()
    }

    pub fn list_ceph_clusters_needing_reconcile(
        &self,
    ) -> Result<Vec<CephClusterRow>, rusqlite::Error> {
        let conn = self.lock_conn()?;
        // Requeue until healthy: lagging observed_generation OR non-terminal phase.
        let mut stmt = conn.prepare(
            "SELECT c.name, c.generation, c.spec_json, COALESCE(c.bootstrap_json, ''),
                    c.created_at, c.updated_at
             FROM ceph_clusters c LEFT JOIN ceph_cluster_status s ON s.name=c.name
             WHERE s.name IS NULL
                OR s.observed_generation < c.generation
                OR s.phase IN ('pending', 'bootstrapping', 'degraded')
             ORDER BY c.name",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok(CephClusterRow {
                name: r.get(0)?,
                generation: r.get(1)?,
                spec_json: r.get(2)?,
                bootstrap_json: r.get(3)?,
                created_at: r.get(4)?,
                updated_at: r.get(5)?,
            })
        })?;
        rows.collect()
    }

    pub fn upsert_shared_filesystem(
        &self,
        row: &SharedFilesystemRow,
    ) -> Result<(), rusqlite::Error> {
        let conn = self.lock_conn()?;
        conn.execute(
            "INSERT INTO shared_filesystems (name, generation, spec_json, created_at, updated_at)
             VALUES (?1, ?2, ?3, COALESCE(NULLIF(?4, ''), datetime('now')), datetime('now'))
             ON CONFLICT(name) DO UPDATE SET
               generation = excluded.generation,
               spec_json = excluded.spec_json,
               updated_at = datetime('now')",
            params![row.name, row.generation, row.spec_json, row.created_at],
        )?;
        Ok(())
    }

    pub fn get_shared_filesystem(
        &self,
        name: &str,
    ) -> Result<Option<SharedFilesystemRow>, rusqlite::Error> {
        let conn = self.lock_conn()?;
        let mut stmt = conn.prepare(
            "SELECT name, generation, spec_json, created_at, updated_at
             FROM shared_filesystems WHERE name = ?1",
        )?;
        let mut rows = stmt.query_map(params![name], |r| {
            Ok(SharedFilesystemRow {
                name: r.get(0)?,
                generation: r.get(1)?,
                spec_json: r.get(2)?,
                created_at: r.get(3)?,
                updated_at: r.get(4)?,
            })
        })?;
        rows.next().transpose()
    }

    pub fn list_shared_filesystems(&self) -> Result<Vec<SharedFilesystemRow>, rusqlite::Error> {
        let conn = self.lock_conn()?;
        let mut stmt = conn.prepare(
            "SELECT name, generation, spec_json, created_at, updated_at
             FROM shared_filesystems ORDER BY name",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok(SharedFilesystemRow {
                name: r.get(0)?,
                generation: r.get(1)?,
                spec_json: r.get(2)?,
                created_at: r.get(3)?,
                updated_at: r.get(4)?,
            })
        })?;
        rows.collect()
    }

    pub fn delete_shared_filesystem(&self, name: &str) -> Result<bool, rusqlite::Error> {
        let conn = self.lock_conn()?;
        Ok(conn.execute(
            "DELETE FROM shared_filesystems WHERE name = ?1",
            params![name],
        )? > 0)
    }

    pub fn upsert_shared_filesystem_status(
        &self,
        row: &SharedFilesystemStatusRow,
    ) -> Result<(), rusqlite::Error> {
        let conn = self.lock_conn()?;
        conn.execute(
            "INSERT INTO shared_filesystem_status (
                name, observed_generation, phase, health_message, last_transition_at
             ) VALUES (?1, ?2, ?3, ?4, COALESCE(NULLIF(?5, ''), datetime('now')))
             ON CONFLICT(name) DO UPDATE SET
               observed_generation = excluded.observed_generation,
               phase = excluded.phase,
               health_message = excluded.health_message,
               last_transition_at = excluded.last_transition_at",
            params![
                row.name,
                row.observed_generation,
                row.phase,
                row.health_message,
                row.last_transition_at
            ],
        )?;
        Ok(())
    }

    pub fn get_shared_filesystem_status(
        &self,
        name: &str,
    ) -> Result<Option<SharedFilesystemStatusRow>, rusqlite::Error> {
        let conn = self.lock_conn()?;
        let mut stmt = conn.prepare(
            "SELECT name, observed_generation, phase, health_message, last_transition_at
             FROM shared_filesystem_status WHERE name = ?1",
        )?;
        let mut rows = stmt.query_map(params![name], |r| {
            Ok(SharedFilesystemStatusRow {
                name: r.get(0)?,
                observed_generation: r.get(1)?,
                phase: r.get(2)?,
                health_message: r.get(3)?,
                last_transition_at: r.get(4)?,
            })
        })?;
        rows.next().transpose()
    }

    pub fn list_shared_filesystems_needing_reconcile(
        &self,
    ) -> Result<Vec<SharedFilesystemRow>, rusqlite::Error> {
        let conn = self.lock_conn()?;
        let mut stmt = conn.prepare(
            "SELECT f.name, f.generation, f.spec_json, f.created_at, f.updated_at
             FROM shared_filesystems f
             LEFT JOIN shared_filesystem_status s ON s.name = f.name
             WHERE s.name IS NULL
                OR s.observed_generation < f.generation
                OR s.phase IN ('pending', 'bootstrapping', 'degraded')
             ORDER BY f.name",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok(SharedFilesystemRow {
                name: r.get(0)?,
                generation: r.get(1)?,
                spec_json: r.get(2)?,
                created_at: r.get(3)?,
                updated_at: r.get(4)?,
            })
        })?;
        rows.collect()
    }

    pub fn upsert_object_store(&self, row: &ObjectStoreRow) -> Result<(), rusqlite::Error> {
        let conn = self.lock_conn()?;
        conn.execute(
            "INSERT INTO object_stores (name, generation, spec_json, created_at, updated_at)
             VALUES (?1, ?2, ?3, COALESCE(NULLIF(?4, ''), datetime('now')), datetime('now'))
             ON CONFLICT(name) DO UPDATE SET
               generation = excluded.generation,
               spec_json = excluded.spec_json,
               updated_at = datetime('now')",
            params![row.name, row.generation, row.spec_json, row.created_at],
        )?;
        Ok(())
    }

    pub fn get_object_store(&self, name: &str) -> Result<Option<ObjectStoreRow>, rusqlite::Error> {
        let conn = self.lock_conn()?;
        let mut stmt = conn.prepare(
            "SELECT name, generation, spec_json, created_at, updated_at
             FROM object_stores WHERE name = ?1",
        )?;
        let mut rows = stmt.query_map(params![name], |r| {
            Ok(ObjectStoreRow {
                name: r.get(0)?,
                generation: r.get(1)?,
                spec_json: r.get(2)?,
                created_at: r.get(3)?,
                updated_at: r.get(4)?,
            })
        })?;
        rows.next().transpose()
    }

    pub fn list_object_stores(&self) -> Result<Vec<ObjectStoreRow>, rusqlite::Error> {
        let conn = self.lock_conn()?;
        let mut stmt = conn.prepare(
            "SELECT name, generation, spec_json, created_at, updated_at
             FROM object_stores ORDER BY name",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok(ObjectStoreRow {
                name: r.get(0)?,
                generation: r.get(1)?,
                spec_json: r.get(2)?,
                created_at: r.get(3)?,
                updated_at: r.get(4)?,
            })
        })?;
        rows.collect()
    }

    pub fn delete_object_store(&self, name: &str) -> Result<bool, rusqlite::Error> {
        let conn = self.lock_conn()?;
        Ok(conn.execute("DELETE FROM object_stores WHERE name = ?1", params![name])? > 0)
    }

    pub fn upsert_object_store_status(
        &self,
        row: &ObjectStoreStatusRow,
    ) -> Result<(), rusqlite::Error> {
        let conn = self.lock_conn()?;
        conn.execute(
            "INSERT INTO object_store_status (
                name, observed_generation, phase, health_message, last_transition_at
             ) VALUES (?1, ?2, ?3, ?4, COALESCE(NULLIF(?5, ''), datetime('now')))
             ON CONFLICT(name) DO UPDATE SET
               observed_generation = excluded.observed_generation,
               phase = excluded.phase,
               health_message = excluded.health_message,
               last_transition_at = excluded.last_transition_at",
            params![
                row.name,
                row.observed_generation,
                row.phase,
                row.health_message,
                row.last_transition_at
            ],
        )?;
        Ok(())
    }

    pub fn get_object_store_status(
        &self,
        name: &str,
    ) -> Result<Option<ObjectStoreStatusRow>, rusqlite::Error> {
        let conn = self.lock_conn()?;
        let mut stmt = conn.prepare(
            "SELECT name, observed_generation, phase, health_message, last_transition_at
             FROM object_store_status WHERE name = ?1",
        )?;
        let mut rows = stmt.query_map(params![name], |r| {
            Ok(ObjectStoreStatusRow {
                name: r.get(0)?,
                observed_generation: r.get(1)?,
                phase: r.get(2)?,
                health_message: r.get(3)?,
                last_transition_at: r.get(4)?,
            })
        })?;
        rows.next().transpose()
    }

    pub fn list_object_stores_needing_reconcile(
        &self,
    ) -> Result<Vec<ObjectStoreRow>, rusqlite::Error> {
        let conn = self.lock_conn()?;
        let mut stmt = conn.prepare(
            "SELECT o.name, o.generation, o.spec_json, o.created_at, o.updated_at
             FROM object_stores o
             LEFT JOIN object_store_status s ON s.name = o.name
             WHERE s.name IS NULL
                OR s.observed_generation < o.generation
                OR s.phase IN ('pending', 'bootstrapping', 'degraded')
             ORDER BY o.name",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok(ObjectStoreRow {
                name: r.get(0)?,
                generation: r.get(1)?,
                spec_json: r.get(2)?,
                created_at: r.get(3)?,
                updated_at: r.get(4)?,
            })
        })?;
        rows.collect()
    }

    pub fn insert_object_user(&self, row: &ObjectUserRow) -> Result<(), rusqlite::Error> {
        let conn = self.lock_conn()?;
        conn.execute(
            "INSERT INTO object_users (name, store_name, access_key, secret, created_at)
             VALUES (?1, ?2, ?3, ?4, COALESCE(NULLIF(?5, ''), datetime('now')))",
            params![
                row.name,
                row.store_name,
                row.access_key,
                row.secret,
                row.created_at
            ],
        )?;
        Ok(())
    }

    pub fn get_object_user(
        &self,
        name: &str,
        store_name: &str,
    ) -> Result<Option<ObjectUserRow>, rusqlite::Error> {
        let conn = self.lock_conn()?;
        let mut stmt = conn.prepare(
            "SELECT name, store_name, access_key, secret, created_at
             FROM object_users WHERE name = ?1 AND store_name = ?2",
        )?;
        let mut rows = stmt.query_map(params![name, store_name], |r| {
            Ok(ObjectUserRow {
                name: r.get(0)?,
                store_name: r.get(1)?,
                access_key: r.get(2)?,
                secret: r.get(3)?,
                created_at: r.get(4)?,
            })
        })?;
        rows.next().transpose()
    }

    pub fn list_object_users_for_store(
        &self,
        store_name: &str,
    ) -> Result<Vec<ObjectUserRow>, rusqlite::Error> {
        let conn = self.lock_conn()?;
        let mut stmt = conn.prepare(
            "SELECT name, store_name, access_key, secret, created_at
             FROM object_users WHERE store_name = ?1 ORDER BY name",
        )?;
        let rows = stmt.query_map(params![store_name], |r| {
            Ok(ObjectUserRow {
                name: r.get(0)?,
                store_name: r.get(1)?,
                access_key: r.get(2)?,
                secret: r.get(3)?,
                created_at: r.get(4)?,
            })
        })?;
        rows.collect()
    }

    pub fn delete_object_user(
        &self,
        name: &str,
        store_name: &str,
    ) -> Result<bool, rusqlite::Error> {
        let conn = self.lock_conn()?;
        Ok(conn.execute(
            "DELETE FROM object_users WHERE name = ?1 AND store_name = ?2",
            params![name, store_name],
        )? > 0)
    }

    pub fn upsert_volume(&self, row: &VolumeRow) -> Result<(), rusqlite::Error> {
        let conn = self.lock_conn()?;
        conn.execute(
            "INSERT INTO volumes (
                id, name, vm_id, role, slot, pool, image, size_bytes,
                storage_class, attach_state, serial, source_json,
                guest_format_json, generation, parent_snapshot_id,
                guest_visible_bytes, guest_checked_at, encrypted, wrapped_dek,
                created_at
             )
             VALUES (
                ?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8,
                ?9, ?10, ?11, ?12, ?13, ?14, ?15,
                ?16, ?17, ?18, ?19,
                COALESCE(NULLIF(?20, ''), datetime('now'))
             )
             ON CONFLICT(id) DO UPDATE SET
               name=excluded.name,
               vm_id=excluded.vm_id,
               role=excluded.role,
               slot=excluded.slot,
               pool=excluded.pool,
               image=excluded.image,
               size_bytes=excluded.size_bytes,
               storage_class=excluded.storage_class,
               attach_state=excluded.attach_state,
               serial=excluded.serial,
               source_json=excluded.source_json,
               guest_format_json=excluded.guest_format_json,
               generation=excluded.generation,
               parent_snapshot_id=excluded.parent_snapshot_id,
               guest_visible_bytes=excluded.guest_visible_bytes,
               guest_checked_at=excluded.guest_checked_at,
               encrypted=excluded.encrypted,
               wrapped_dek=excluded.wrapped_dek",
            params![
                row.id,
                row.name,
                row.vm_id,
                row.role,
                row.slot,
                row.pool,
                row.image,
                row.size_bytes,
                row.storage_class,
                row.attach_state,
                row.serial,
                row.source_json,
                row.guest_format_json,
                row.generation,
                row.parent_snapshot_id,
                row.guest_visible_bytes,
                row.guest_checked_at,
                row.encrypted as i32,
                row.wrapped_dek,
                row.created_at
            ],
        )?;
        Ok(())
    }

    fn map_volume_row(r: &rusqlite::Row<'_>) -> Result<VolumeRow, rusqlite::Error> {
        Ok(VolumeRow {
            id: r.get(0)?,
            name: r.get(1)?,
            vm_id: r.get(2)?,
            role: r.get(3)?,
            slot: r.get(4)?,
            pool: r.get(5)?,
            image: r.get(6)?,
            size_bytes: r.get(7)?,
            storage_class: r.get(8)?,
            attach_state: r.get(9)?,
            serial: r.get(10)?,
            source_json: r.get(11)?,
            guest_format_json: r.get(12)?,
            generation: r.get(13)?,
            parent_snapshot_id: r.get(14)?,
            guest_visible_bytes: r.get(15)?,
            guest_checked_at: r.get(16)?,
            encrypted: r.get::<_, i32>(17)? != 0,
            wrapped_dek: r.get(18)?,
            created_at: r.get(19)?,
        })
    }

    const VOLUME_SELECT: &'static str =
        "SELECT id, name, vm_id, role, slot, pool, image, size_bytes,
            storage_class, attach_state, serial, source_json, guest_format_json,
            generation, parent_snapshot_id, guest_visible_bytes, guest_checked_at,
            encrypted, wrapped_dek, created_at FROM volumes";

    /// Root volume for a VM (attach_state may still be attached). None when the
    /// VM has no Ceph root row yet.
    pub fn get_volume_by_vm(&self, vm_id: &str) -> Result<Option<VolumeRow>, rusqlite::Error> {
        let conn = self.lock_conn()?;
        let mut stmt = conn.prepare(&format!(
            "{} WHERE vm_id=?1 AND role='root' ORDER BY slot LIMIT 1",
            Self::VOLUME_SELECT
        ))?;
        let mut rows = stmt.query_map(params![vm_id], Self::map_volume_row)?;
        rows.next().transpose()
    }

    pub fn get_volume_by_id(&self, id: &str) -> Result<Option<VolumeRow>, rusqlite::Error> {
        let conn = self.lock_conn()?;
        let mut stmt = conn.prepare(&format!("{} WHERE id=?1", Self::VOLUME_SELECT))?;
        let mut rows = stmt.query_map(params![id], Self::map_volume_row)?;
        rows.next().transpose()
    }

    pub fn get_volume_by_name(&self, name: &str) -> Result<Option<VolumeRow>, rusqlite::Error> {
        let conn = self.lock_conn()?;
        let mut stmt = conn.prepare(&format!("{} WHERE name=?1", Self::VOLUME_SELECT))?;
        let mut rows = stmt.query_map(params![name], Self::map_volume_row)?;
        rows.next().transpose()
    }

    pub fn list_volumes_for_vm(&self, vm_id: &str) -> Result<Vec<VolumeRow>, rusqlite::Error> {
        let conn = self.lock_conn()?;
        let mut stmt = conn.prepare(&format!(
            "{} WHERE vm_id=?1 ORDER BY role ASC, slot ASC, name ASC",
            Self::VOLUME_SELECT
        ))?;
        let rows = stmt.query_map(params![vm_id], Self::map_volume_row)?;
        rows.collect()
    }

    /// Volumes that must be mapped for a running or migrating VM.
    pub fn list_attached_volumes_for_vm(
        &self,
        vm_id: &str,
    ) -> Result<Vec<VolumeRow>, rusqlite::Error> {
        let conn = self.lock_conn()?;
        let mut stmt = conn.prepare(&format!(
            "{} WHERE vm_id=?1 AND attach_state IN ('attached', 'attaching')
             ORDER BY role ASC, slot ASC, name ASC",
            Self::VOLUME_SELECT
        ))?;
        let rows = stmt.query_map(params![vm_id], Self::map_volume_row)?;
        rows.collect()
    }

    pub fn next_data_slot_for_vm(&self, vm_id: &str) -> Result<i32, rusqlite::Error> {
        let conn = self.lock_conn()?;
        let max: Option<i32> = conn.query_row(
            "SELECT MAX(slot) FROM volumes WHERE vm_id=?1 AND role='data'",
            params![vm_id],
            |r| r.get(0),
        )?;
        Ok(max.unwrap_or(0) + 1)
    }

    pub fn list_volumes(&self) -> Result<Vec<VolumeRow>, rusqlite::Error> {
        let conn = self.lock_conn()?;
        let mut stmt = conn.prepare(&format!("{} ORDER BY name ASC", Self::VOLUME_SELECT))?;
        let rows = stmt.query_map([], Self::map_volume_row)?;
        rows.collect()
    }

    pub fn delete_volume_by_id(&self, id: &str) -> Result<bool, rusqlite::Error> {
        let conn = self.lock_conn()?;
        Ok(conn.execute("DELETE FROM volumes WHERE id=?1", params![id])? > 0)
    }

    /// Delete every volume row for a VM (root and data). Prefer
    /// `delete_root_and_optionally_data` from DeleteVm.
    pub fn delete_volume_by_vm(&self, vm_id: &str) -> Result<bool, rusqlite::Error> {
        let conn = self.lock_conn()?;
        Ok(conn.execute("DELETE FROM volumes WHERE vm_id=?1", params![vm_id])? > 0)
    }

    pub fn delete_root_volume_by_vm(&self, vm_id: &str) -> Result<bool, rusqlite::Error> {
        let conn = self.lock_conn()?;
        Ok(conn.execute(
            "DELETE FROM volumes WHERE vm_id=?1 AND role='root'",
            params![vm_id],
        )? > 0)
    }

    /// Detach data volumes (clear vm_id, attach_state=detached) without deleting RBD.
    pub fn detach_data_volumes_for_vm(&self, vm_id: &str) -> Result<usize, rusqlite::Error> {
        let conn = self.lock_conn()?;
        Ok(conn.execute(
            "UPDATE volumes SET vm_id='', attach_state='detached', slot=0,
               generation=generation+1
             WHERE vm_id=?1 AND role='data'",
            params![vm_id],
        )?)
    }

    pub fn insert_vm_operation(&self, row: &VmOperationRow) -> Result<(), rusqlite::Error> {
        let conn = self.lock_conn()?;
        conn.execute(
            "INSERT INTO vm_operations (
                id, vm_id, kind, phase, source_node, target_node,
                cancel_requested, send_succeeded, detail_json,
                started_at, updated_at, finished_at
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9,
                COALESCE(NULLIF(?10, ''), datetime('now')),
                datetime('now'), ?11)",
            params![
                row.id,
                row.vm_id,
                row.kind,
                row.phase,
                row.source_node,
                row.target_node,
                row.cancel_requested as i32,
                row.send_succeeded as i32,
                row.detail_json,
                row.started_at,
                row.finished_at,
            ],
        )?;
        Ok(())
    }

    pub fn get_vm_operation(&self, id: &str) -> Result<Option<VmOperationRow>, rusqlite::Error> {
        let conn = self.lock_conn()?;
        let mut stmt = conn.prepare(
            "SELECT id, vm_id, kind, phase, source_node, target_node,
                    cancel_requested, send_succeeded, detail_json,
                    started_at, updated_at, finished_at
             FROM vm_operations WHERE id=?1",
        )?;
        let mut rows = stmt.query_map(params![id], Self::map_vm_operation_row)?;
        rows.next().transpose()
    }

    pub fn get_open_vm_operation(
        &self,
        vm_id: &str,
    ) -> Result<Option<VmOperationRow>, rusqlite::Error> {
        let conn = self.lock_conn()?;
        let mut stmt = conn.prepare(
            "SELECT id, vm_id, kind, phase, source_node, target_node,
                    cancel_requested, send_succeeded, detail_json,
                    started_at, updated_at, finished_at
             FROM vm_operations WHERE vm_id=?1 AND finished_at='' LIMIT 1",
        )?;
        let mut rows = stmt.query_map(params![vm_id], Self::map_vm_operation_row)?;
        rows.next().transpose()
    }

    pub fn list_vm_operations(
        &self,
        include_finished: bool,
    ) -> Result<Vec<VmOperationRow>, rusqlite::Error> {
        let conn = self.lock_conn()?;
        let sql = if include_finished {
            "SELECT id, vm_id, kind, phase, source_node, target_node,
                    cancel_requested, send_succeeded, detail_json,
                    started_at, updated_at, finished_at
             FROM vm_operations ORDER BY started_at DESC"
        } else {
            "SELECT id, vm_id, kind, phase, source_node, target_node,
                    cancel_requested, send_succeeded, detail_json,
                    started_at, updated_at, finished_at
             FROM vm_operations WHERE finished_at='' ORDER BY started_at DESC"
        };
        let mut stmt = conn.prepare(sql)?;
        let rows = stmt.query_map([], Self::map_vm_operation_row)?;
        rows.collect()
    }

    pub fn update_vm_operation_phase(
        &self,
        id: &str,
        phase: &str,
        send_succeeded: Option<bool>,
        detail_json: Option<&str>,
        finished: bool,
    ) -> Result<(), rusqlite::Error> {
        let conn = self.lock_conn()?;
        let finished_at = if finished {
            "datetime('now')"
        } else {
            "finished_at"
        };
        // finished_at expression can't be bound as string for SQL keyword — branch.
        if finished {
            conn.execute(
                "UPDATE vm_operations SET phase=?2,
                   send_succeeded=COALESCE(?3, send_succeeded),
                   detail_json=COALESCE(?4, detail_json),
                   updated_at=datetime('now'),
                   finished_at=datetime('now')
                 WHERE id=?1",
                params![id, phase, send_succeeded.map(|b| b as i32), detail_json,],
            )?;
        } else {
            conn.execute(
                "UPDATE vm_operations SET phase=?2,
                   send_succeeded=COALESCE(?3, send_succeeded),
                   detail_json=COALESCE(?4, detail_json),
                   updated_at=datetime('now')
                 WHERE id=?1",
                params![id, phase, send_succeeded.map(|b| b as i32), detail_json,],
            )?;
        }
        let _ = finished_at;
        Ok(())
    }

    pub fn request_cancel_vm_operation(&self, id: &str) -> Result<bool, rusqlite::Error> {
        let conn = self.lock_conn()?;
        Ok(conn.execute(
            "UPDATE vm_operations SET cancel_requested=1, updated_at=datetime('now')
             WHERE id=?1 AND finished_at=''",
            params![id],
        )? > 0)
    }

    fn map_vm_operation_row(r: &rusqlite::Row<'_>) -> Result<VmOperationRow, rusqlite::Error> {
        Ok(VmOperationRow {
            id: r.get(0)?,
            vm_id: r.get(1)?,
            kind: r.get(2)?,
            phase: r.get(3)?,
            source_node: r.get(4)?,
            target_node: r.get(5)?,
            cancel_requested: r.get::<_, i32>(6)? != 0,
            send_succeeded: r.get::<_, i32>(7)? != 0,
            detail_json: r.get(8)?,
            started_at: r.get(9)?,
            updated_at: r.get(10)?,
            finished_at: r.get(11)?,
        })
    }

    pub fn insert_volume_snapshot(&self, row: &VolumeSnapshotRow) -> Result<(), rusqlite::Error> {
        let conn = self.lock_conn()?;
        conn.execute(
            "INSERT INTO volume_snapshots (
                id, name, volume_id, rbd_snap, protected, size_bytes, consistency, created_at
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7,
                COALESCE(NULLIF(?8, ''), datetime('now')))",
            params![
                row.id,
                row.name,
                row.volume_id,
                row.rbd_snap,
                row.protected as i32,
                row.size_bytes,
                row.consistency,
                row.created_at,
            ],
        )?;
        Ok(())
    }

    pub fn get_volume_snapshot_by_name(
        &self,
        name: &str,
    ) -> Result<Option<VolumeSnapshotRow>, rusqlite::Error> {
        let conn = self.lock_conn()?;
        let mut stmt = conn.prepare(
            "SELECT id, name, volume_id, rbd_snap, protected, size_bytes, consistency, created_at
             FROM volume_snapshots WHERE name=?1",
        )?;
        let mut rows = stmt.query_map(params![name], Self::map_volume_snapshot_row)?;
        rows.next().transpose()
    }

    pub fn get_volume_snapshot_by_id(
        &self,
        id: &str,
    ) -> Result<Option<VolumeSnapshotRow>, rusqlite::Error> {
        let conn = self.lock_conn()?;
        let mut stmt = conn.prepare(
            "SELECT id, name, volume_id, rbd_snap, protected, size_bytes, consistency, created_at
             FROM volume_snapshots WHERE id=?1",
        )?;
        let mut rows = stmt.query_map(params![id], Self::map_volume_snapshot_row)?;
        rows.next().transpose()
    }

    pub fn list_volume_snapshots(
        &self,
        volume_id: Option<&str>,
    ) -> Result<Vec<VolumeSnapshotRow>, rusqlite::Error> {
        let conn = self.lock_conn()?;
        if let Some(vid) = volume_id {
            let mut stmt = conn.prepare(
                "SELECT id, name, volume_id, rbd_snap, protected, size_bytes, consistency, created_at
                 FROM volume_snapshots WHERE volume_id=?1 ORDER BY created_at DESC",
            )?;
            let rows = stmt.query_map(params![vid], Self::map_volume_snapshot_row)?;
            rows.collect()
        } else {
            let mut stmt = conn.prepare(
                "SELECT id, name, volume_id, rbd_snap, protected, size_bytes, consistency, created_at
                 FROM volume_snapshots ORDER BY created_at DESC",
            )?;
            let rows = stmt.query_map([], Self::map_volume_snapshot_row)?;
            rows.collect()
        }
    }

    pub fn count_volumes_with_parent_snapshot(
        &self,
        snapshot_id: &str,
    ) -> Result<i64, rusqlite::Error> {
        let conn = self.lock_conn()?;
        conn.query_row(
            "SELECT COUNT(*) FROM volumes WHERE parent_snapshot_id=?1",
            params![snapshot_id],
            |r| r.get(0),
        )
    }

    pub fn delete_volume_snapshot(&self, id: &str) -> Result<bool, rusqlite::Error> {
        let conn = self.lock_conn()?;
        Ok(conn.execute("DELETE FROM volume_snapshots WHERE id=?1", params![id])? > 0)
    }

    fn map_volume_snapshot_row(
        r: &rusqlite::Row<'_>,
    ) -> Result<VolumeSnapshotRow, rusqlite::Error> {
        Ok(VolumeSnapshotRow {
            id: r.get(0)?,
            name: r.get(1)?,
            volume_id: r.get(2)?,
            rbd_snap: r.get(3)?,
            protected: r.get::<_, i32>(4)? != 0,
            size_bytes: r.get(5)?,
            consistency: r.get(6)?,
            created_at: r.get(7)?,
        })
    }

    pub fn upsert_snapshot_policy(&self, row: &SnapshotPolicyRow) -> Result<(), rusqlite::Error> {
        let conn = self.lock_conn()?;
        conn.execute(
            "INSERT INTO snapshot_policies (
                name, selector_vm, selector_volume, schedule, keep, enabled,
                last_run_at, last_message, created_at, updated_at
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8,
                COALESCE(NULLIF(?9, ''), datetime('now')), datetime('now'))
             ON CONFLICT(name) DO UPDATE SET
               selector_vm=excluded.selector_vm,
               selector_volume=excluded.selector_volume,
               schedule=excluded.schedule,
               keep=excluded.keep,
               enabled=excluded.enabled,
               last_run_at=excluded.last_run_at,
               last_message=excluded.last_message,
               updated_at=datetime('now')",
            params![
                row.name,
                row.selector_vm,
                row.selector_volume,
                row.schedule,
                row.keep,
                row.enabled as i32,
                row.last_run_at,
                row.last_message,
                row.created_at,
            ],
        )?;
        Ok(())
    }

    pub fn get_snapshot_policy(
        &self,
        name: &str,
    ) -> Result<Option<SnapshotPolicyRow>, rusqlite::Error> {
        let conn = self.lock_conn()?;
        let mut stmt = conn.prepare(
            "SELECT name, selector_vm, selector_volume, schedule, keep, enabled,
                    last_run_at, last_message, created_at, updated_at
             FROM snapshot_policies WHERE name=?1",
        )?;
        let mut rows = stmt.query_map(params![name], Self::map_snapshot_policy_row)?;
        rows.next().transpose()
    }

    pub fn list_snapshot_policies(&self) -> Result<Vec<SnapshotPolicyRow>, rusqlite::Error> {
        let conn = self.lock_conn()?;
        let mut stmt = conn.prepare(
            "SELECT name, selector_vm, selector_volume, schedule, keep, enabled,
                    last_run_at, last_message, created_at, updated_at
             FROM snapshot_policies ORDER BY name",
        )?;
        let rows = stmt.query_map([], Self::map_snapshot_policy_row)?;
        rows.collect()
    }

    pub fn delete_snapshot_policy(&self, name: &str) -> Result<bool, rusqlite::Error> {
        let conn = self.lock_conn()?;
        Ok(conn.execute("DELETE FROM snapshot_policies WHERE name=?1", params![name])? > 0)
    }

    pub fn upsert_guest_ops_key(
        &self,
        node_id: &str,
        public_key: &str,
    ) -> Result<(), rusqlite::Error> {
        let conn = self.lock_conn()?;
        conn.execute(
            "INSERT INTO guest_ops_node_keys (node_id, public_key, updated_at)
             VALUES (?1, ?2, datetime('now'))
             ON CONFLICT(node_id) DO UPDATE SET
               public_key=excluded.public_key,
               updated_at=datetime('now')",
            params![node_id, public_key],
        )?;
        Ok(())
    }

    pub fn list_guest_ops_keys(&self) -> Result<Vec<(String, String)>, rusqlite::Error> {
        let conn = self.lock_conn()?;
        let mut stmt =
            conn.prepare("SELECT node_id, public_key FROM guest_ops_node_keys ORDER BY node_id")?;
        let rows = stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?;
        rows.collect()
    }

    fn map_snapshot_policy_row(
        r: &rusqlite::Row<'_>,
    ) -> Result<SnapshotPolicyRow, rusqlite::Error> {
        Ok(SnapshotPolicyRow {
            name: r.get(0)?,
            selector_vm: r.get(1)?,
            selector_volume: r.get(2)?,
            schedule: r.get(3)?,
            keep: r.get(4)?,
            enabled: r.get::<_, i32>(5)? != 0,
            last_run_at: r.get(6)?,
            last_message: r.get(7)?,
            created_at: r.get(8)?,
            updated_at: r.get(9)?,
        })
    }

    pub fn upsert_cluster_update(&self, row: &ClusterUpdateRow) -> Result<(), rusqlite::Error> {
        let conn = self.lock_conn()?;
        conn.execute(
            "INSERT INTO cluster_updates (
                name, generation, target_version, flake_ref, flake_rev, spec_json,
                phase, approval_status, created_at, updated_at
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8,
                COALESCE(NULLIF(?9, ''), datetime('now')), datetime('now'))
             ON CONFLICT(name) DO UPDATE SET
               generation = excluded.generation,
               target_version = excluded.target_version,
               flake_ref = excluded.flake_ref,
               flake_rev = excluded.flake_rev,
               spec_json = excluded.spec_json,
               phase = excluded.phase,
               approval_status = excluded.approval_status,
               updated_at = datetime('now')",
            params![
                row.name,
                row.generation,
                row.target_version,
                row.flake_ref,
                row.flake_rev,
                row.spec_json,
                row.phase,
                row.approval_status,
                row.created_at,
            ],
        )?;
        Ok(())
    }

    pub fn get_cluster_update(
        &self,
        name: &str,
    ) -> Result<Option<ClusterUpdateRow>, rusqlite::Error> {
        let conn = self.lock_conn()?;
        let mut stmt = conn.prepare(
            "SELECT name, generation, target_version, flake_ref, flake_rev, spec_json,
                    phase, approval_status, created_at, updated_at
             FROM cluster_updates WHERE name = ?1",
        )?;
        let mut rows = stmt.query_map(params![name], |row| {
            Ok(ClusterUpdateRow {
                name: row.get(0)?,
                generation: row.get(1)?,
                target_version: row.get(2)?,
                flake_ref: row.get(3)?,
                flake_rev: row.get(4)?,
                spec_json: row.get(5)?,
                phase: row.get(6)?,
                approval_status: row.get(7)?,
                created_at: row.get(8)?,
                updated_at: row.get(9)?,
            })
        })?;
        rows.next().transpose()
    }

    pub fn list_cluster_updates(&self) -> Result<Vec<ClusterUpdateRow>, rusqlite::Error> {
        let conn = self.lock_conn()?;
        let mut stmt = conn.prepare(
            "SELECT name, generation, target_version, flake_ref, flake_rev, spec_json,
                    phase, approval_status, created_at, updated_at
             FROM cluster_updates ORDER BY name ASC",
        )?;
        let rows = stmt.query_map([], |row| {
            Ok(ClusterUpdateRow {
                name: row.get(0)?,
                generation: row.get(1)?,
                target_version: row.get(2)?,
                flake_ref: row.get(3)?,
                flake_rev: row.get(4)?,
                spec_json: row.get(5)?,
                phase: row.get(6)?,
                approval_status: row.get(7)?,
                created_at: row.get(8)?,
                updated_at: row.get(9)?,
            })
        })?;
        rows.collect()
    }

    pub fn delete_cluster_update_nodes(&self, update_name: &str) -> Result<(), rusqlite::Error> {
        let conn = self.lock_conn()?;
        conn.execute(
            "DELETE FROM cluster_update_nodes WHERE update_name = ?1",
            params![update_name],
        )?;
        Ok(())
    }

    pub fn replace_cluster_update_nodes(
        &self,
        update_name: &str,
        rows: &[ClusterUpdateNodeRow],
    ) -> Result<(), rusqlite::Error> {
        let conn = self.lock_conn()?;
        let tx = conn.unchecked_transaction()?;
        tx.execute(
            "DELETE FROM cluster_update_nodes WHERE update_name = ?1",
            params![update_name],
        )?;
        for r in rows {
            tx.execute(
                "INSERT INTO cluster_update_nodes (
                    update_name, node_id, observed_generation, phase,
                    current_version, target_version, prepared_closure,
                    current_generation, target_generation, requires_reboot,
                    last_error, last_transition_at
                 ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
                params![
                    r.update_name,
                    r.node_id,
                    r.observed_generation,
                    r.phase,
                    r.current_version,
                    r.target_version,
                    r.prepared_closure,
                    r.current_generation,
                    r.target_generation,
                    r.requires_reboot as i32,
                    r.last_error,
                    r.last_transition_at,
                ],
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    pub fn upsert_cluster_update_node(
        &self,
        row: &ClusterUpdateNodeRow,
    ) -> Result<(), rusqlite::Error> {
        let conn = self.lock_conn()?;
        conn.execute(
            "INSERT INTO cluster_update_nodes (
                update_name, node_id, observed_generation, phase,
                current_version, target_version, prepared_closure,
                current_generation, target_generation, requires_reboot,
                last_error, last_transition_at
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)
             ON CONFLICT(update_name, node_id) DO UPDATE SET
               observed_generation = excluded.observed_generation,
               phase = excluded.phase,
               current_version = excluded.current_version,
               target_version = excluded.target_version,
               prepared_closure = excluded.prepared_closure,
               current_generation = excluded.current_generation,
               target_generation = excluded.target_generation,
               requires_reboot = excluded.requires_reboot,
               last_error = excluded.last_error,
               last_transition_at = datetime('now')",
            params![
                row.update_name,
                row.node_id,
                row.observed_generation,
                row.phase,
                row.current_version,
                row.target_version,
                row.prepared_closure,
                row.current_generation,
                row.target_generation,
                row.requires_reboot as i32,
                row.last_error,
                row.last_transition_at,
            ],
        )?;
        Ok(())
    }

    pub fn list_cluster_update_nodes(
        &self,
        update_name: &str,
    ) -> Result<Vec<ClusterUpdateNodeRow>, rusqlite::Error> {
        let conn = self.lock_conn()?;
        let mut stmt = conn.prepare(
            "SELECT update_name, node_id, observed_generation, phase,
                    current_version, target_version, prepared_closure,
                    current_generation, target_generation, requires_reboot,
                    last_error, last_transition_at
             FROM cluster_update_nodes
             WHERE update_name = ?1
             ORDER BY node_id ASC",
        )?;
        let rows = stmt.query_map(params![update_name], |row| {
            Ok(ClusterUpdateNodeRow {
                update_name: row.get(0)?,
                node_id: row.get(1)?,
                observed_generation: row.get(2)?,
                phase: row.get(3)?,
                current_version: row.get(4)?,
                target_version: row.get(5)?,
                prepared_closure: row.get(6)?,
                current_generation: row.get(7)?,
                target_generation: row.get(8)?,
                requires_reboot: row.get::<_, i32>(9)? != 0,
                last_error: row.get(10)?,
                last_transition_at: row.get(11)?,
            })
        })?;
        rows.collect()
    }

    pub fn list_cluster_updates_for_reconcile(
        &self,
    ) -> Result<Vec<ClusterUpdateRow>, rusqlite::Error> {
        let conn = self.lock_conn()?;
        let mut stmt = conn.prepare(
            "SELECT name, generation, target_version, flake_ref, flake_rev, spec_json,
                    phase, approval_status, created_at, updated_at
             FROM cluster_updates
             WHERE phase IN ('ready', 'rolling_out', 'rolling_back')
             ORDER BY name ASC",
        )?;
        let rows = stmt.query_map([], |row| {
            Ok(ClusterUpdateRow {
                name: row.get(0)?,
                generation: row.get(1)?,
                target_version: row.get(2)?,
                flake_ref: row.get(3)?,
                flake_rev: row.get(4)?,
                spec_json: row.get(5)?,
                phase: row.get(6)?,
                approval_status: row.get(7)?,
                created_at: row.get(8)?,
                updated_at: row.get(9)?,
            })
        })?;
        rows.collect()
    }

    pub fn patch_cluster_update_phase(
        &self,
        name: &str,
        phase: &str,
    ) -> Result<(), rusqlite::Error> {
        let conn = self.lock_conn()?;
        conn.execute(
            "UPDATE cluster_updates SET phase = ?2, updated_at = datetime('now') WHERE name = ?1",
            params![name, phase],
        )?;
        Ok(())
    }

    pub fn replace_security_group_rules(
        &self,
        security_group: &str,
        rules: &[SecurityGroupRuleRow],
    ) -> Result<(), rusqlite::Error> {
        let conn = self.lock_conn()?;
        let tx = conn.unchecked_transaction()?;
        tx.execute(
            "DELETE FROM security_group_rules WHERE security_group = ?1",
            params![security_group],
        )?;
        for rule in rules {
            tx.execute(
                "INSERT INTO security_group_rules (id, security_group, protocol, host_port, target_port, source_cidr, target_vm, enable_dnat)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                params![
                    rule.id,
                    security_group,
                    rule.protocol,
                    rule.host_port,
                    rule.target_port,
                    rule.source_cidr,
                    rule.target_vm,
                    rule.enable_dnat as i32
                ],
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    pub fn list_security_group_rules(
        &self,
        security_group: &str,
    ) -> Result<Vec<SecurityGroupRuleRow>, rusqlite::Error> {
        let conn = self.lock_conn()?;
        let mut stmt = conn.prepare(
            "SELECT id, security_group, protocol, host_port, target_port, source_cidr, target_vm, enable_dnat
             FROM security_group_rules
             WHERE security_group = ?1
             ORDER BY id ASC",
        )?;
        let rows = stmt.query_map(params![security_group], |row| {
            Ok(SecurityGroupRuleRow {
                id: row.get(0)?,
                security_group: row.get(1)?,
                protocol: row.get(2)?,
                host_port: row.get(3)?,
                target_port: row.get(4)?,
                source_cidr: row.get(5)?,
                target_vm: row.get(6)?,
                enable_dnat: row.get::<_, i32>(7)? != 0,
            })
        })?;
        rows.collect()
    }

    pub fn attach_security_group_to_vm(
        &self,
        security_group: &str,
        vm_id: &str,
    ) -> Result<(), rusqlite::Error> {
        let conn = self.lock_conn()?;
        conn.execute(
            "INSERT OR IGNORE INTO security_group_vm_attachments (security_group, vm_id)
             VALUES (?1, ?2)",
            params![security_group, vm_id],
        )?;
        Ok(())
    }

    pub fn detach_security_group_from_vm(
        &self,
        security_group: &str,
        vm_id: &str,
    ) -> Result<bool, rusqlite::Error> {
        let conn = self.lock_conn()?;
        let rows = conn.execute(
            "DELETE FROM security_group_vm_attachments WHERE security_group = ?1 AND vm_id = ?2",
            params![security_group, vm_id],
        )?;
        Ok(rows > 0)
    }

    pub fn list_security_group_vm_attachments(
        &self,
        security_group: &str,
    ) -> Result<Vec<SecurityGroupVmAttachmentRow>, rusqlite::Error> {
        let conn = self.lock_conn()?;
        let mut stmt = conn.prepare(
            "SELECT security_group, vm_id
             FROM security_group_vm_attachments
             WHERE security_group = ?1
             ORDER BY vm_id ASC",
        )?;
        let rows = stmt.query_map(params![security_group], |row| {
            Ok(SecurityGroupVmAttachmentRow {
                security_group: row.get(0)?,
                vm_id: row.get(1)?,
            })
        })?;
        rows.collect()
    }

    pub fn list_security_groups_for_vm(&self, vm_id: &str) -> Result<Vec<String>, rusqlite::Error> {
        let conn = self.lock_conn()?;
        let mut stmt = conn.prepare(
            "SELECT security_group FROM security_group_vm_attachments WHERE vm_id = ?1 ORDER BY security_group ASC",
        )?;
        let rows = stmt.query_map(params![vm_id], |row| row.get(0))?;
        rows.collect()
    }

    pub fn attach_security_group_to_network(
        &self,
        security_group: &str,
        network_name: &str,
        node_id: &str,
    ) -> Result<(), rusqlite::Error> {
        let conn = self.lock_conn()?;
        conn.execute(
            "INSERT OR IGNORE INTO security_group_network_attachments (security_group, network_name, node_id)
             VALUES (?1, ?2, ?3)",
            params![security_group, network_name, node_id],
        )?;
        Ok(())
    }

    pub fn detach_security_group_from_network(
        &self,
        security_group: &str,
        network_name: &str,
        node_id: &str,
    ) -> Result<bool, rusqlite::Error> {
        let conn = self.lock_conn()?;
        let rows = conn.execute(
            "DELETE FROM security_group_network_attachments
             WHERE security_group = ?1 AND network_name = ?2 AND node_id = ?3",
            params![security_group, network_name, node_id],
        )?;
        Ok(rows > 0)
    }

    pub fn list_security_group_network_attachments(
        &self,
        security_group: &str,
    ) -> Result<Vec<SecurityGroupNetworkAttachmentRow>, rusqlite::Error> {
        let conn = self.lock_conn()?;
        let mut stmt = conn.prepare(
            "SELECT security_group, network_name, node_id
             FROM security_group_network_attachments
             WHERE security_group = ?1
             ORDER BY node_id ASC, network_name ASC",
        )?;
        let rows = stmt.query_map(params![security_group], |row| {
            Ok(SecurityGroupNetworkAttachmentRow {
                security_group: row.get(0)?,
                network_name: row.get(1)?,
                node_id: row.get(2)?,
            })
        })?;
        rows.collect()
    }

    pub fn list_security_groups_for_network(
        &self,
        network_name: &str,
        node_id: &str,
    ) -> Result<Vec<String>, rusqlite::Error> {
        let conn = self.lock_conn()?;
        let mut stmt = conn.prepare(
            "SELECT security_group FROM security_group_network_attachments
             WHERE network_name = ?1 AND node_id = ?2
             ORDER BY security_group ASC",
        )?;
        let rows = stmt.query_map(params![network_name, node_id], |row| row.get(0))?;
        rows.collect()
    }

    pub fn find_node_for_vm(&self, vm_id: &str) -> Result<Option<String>, rusqlite::Error> {
        let conn = self.lock_conn()?;
        let mut stmt =
            conn.prepare("SELECT node_id FROM vms WHERE id = ?1 OR name = ?1 LIMIT 1")?;
        let mut rows = stmt.query_map(params![vm_id], |row| row.get::<_, String>(0))?;
        rows.next().transpose()
    }

    pub fn set_vm_auto_start(
        &self,
        vm_id_or_name: &str,
        auto_start: bool,
    ) -> Result<bool, rusqlite::Error> {
        let conn = self.lock_conn()?;
        let rows = conn.execute(
            "UPDATE vms SET auto_start = ?1 WHERE id = ?2 OR name = ?2",
            params![auto_start as i32, vm_id_or_name],
        )?;
        Ok(rows > 0)
    }

    /// Compare-and-swap for `auto_start`: set to `new_value` only if the row
    /// currently equals `expected_current`. Used to roll back a failed node
    /// push without clobbering a concurrent successful update.
    pub fn set_vm_auto_start_if_current(
        &self,
        vm_id_or_name: &str,
        expected_current: bool,
        new_value: bool,
    ) -> Result<bool, rusqlite::Error> {
        let conn = self.lock_conn()?;
        let rows = conn.execute(
            "UPDATE vms SET auto_start = ?1 WHERE (id = ?2 OR name = ?2) AND auto_start = ?3",
            params![new_value as i32, vm_id_or_name, expected_current as i32,],
        )?;
        Ok(rows > 0)
    }

    pub fn update_vm_runtime_state(
        &self,
        node_id: &str,
        vm_name: &str,
        state: &str,
    ) -> Result<bool, rusqlite::Error> {
        let conn = self.lock_conn()?;
        let rows = conn.execute(
            "UPDATE vms SET runtime_state = ?1 WHERE name = ?2 AND node_id = ?3",
            params![state, vm_name, node_id],
        )?;
        Ok(rows > 0)
    }

    /// Write `state` only when it differs from the stored runtime state.
    ///
    /// `Missing` means this node does not track that VM name. `Unchanged`
    /// means the row exists and already has `state`, so callers must not treat
    /// it as an orphan or as a transition.
    pub fn apply_vm_runtime_state(
        &self,
        node_id: &str,
        vm_name: &str,
        state: &str,
    ) -> Result<VmRuntimeApply, rusqlite::Error> {
        let conn = self.lock_conn()?;
        let found = conn.query_row(
            "SELECT id, name, node_id, runtime_state FROM vms WHERE name = ?1 AND node_id = ?2",
            params![vm_name, node_id],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                ))
            },
        );
        let (vm_id, name, stored_node, previous) = match found {
            Ok(row) => row,
            Err(rusqlite::Error::QueryReturnedNoRows) => return Ok(VmRuntimeApply::Missing),
            Err(err) => return Err(err),
        };
        if previous == state {
            return Ok(VmRuntimeApply::Unchanged);
        }
        let rows = conn.execute(
            "UPDATE vms SET runtime_state = ?1 WHERE id = ?2 AND runtime_state = ?3",
            params![state, vm_id, previous],
        )?;
        if rows == 0 {
            return Ok(VmRuntimeApply::Unchanged);
        }
        Ok(VmRuntimeApply::Changed(VmRuntimeChange {
            vm_id,
            name,
            node_id: stored_node,
            previous,
            current: state.to_string(),
        }))
    }

    pub fn delete_vm_by_id_or_name(&self, id_or_name: &str) -> Result<bool, rusqlite::Error> {
        let mut conn = self.lock_conn()?;
        let tx = conn.transaction()?;
        let found = {
            let mut stmt = tx.prepare(
                "SELECT id, node_id, network, vm_ip FROM vms WHERE id = ?1 OR name = ?1",
            )?;
            let rows = stmt.query_map(params![id_or_name], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                ))
            })?;
            let found: Vec<(String, String, String, String)> =
                rows.collect::<Result<Vec<_>, _>>()?;
            found
        };
        if found.is_empty() {
            return Ok(false);
        }
        for (vm_id, node_id, network, vm_ip) in &found {
            release_vm_addresses(&tx, vm_id, node_id, network, vm_ip)?;
        }
        let rows = tx.execute(
            "DELETE FROM vms WHERE id = ?1 OR name = ?1",
            params![id_or_name],
        )?;
        tx.commit()?;
        Ok(rows > 0)
    }

    /// Move a VM to another node in place.
    ///
    /// Every table keyed on `vms(id)` cascades on delete under
    /// `PRAGMA foreign_keys=ON` (`vm_ssh_keys`,
    /// `security_group_vm_attachments`), so reassignment must never be
    /// expressed as delete-then-reinsert. Returns `false` when no such VM
    /// exists; a missing target node is rejected by the `node_id` foreign key.
    pub fn set_vm_pci_devices(
        &self,
        vm_id: &str,
        pci_devices: &str,
    ) -> Result<bool, rusqlite::Error> {
        let conn = self.lock_conn()?;
        let rows = conn.execute(
            "UPDATE vms SET pci_devices = ?2 WHERE id = ?1",
            params![vm_id, pci_devices],
        )?;
        Ok(rows > 0)
    }

    pub fn set_vm_gpu(&self, vm_id: &str, gpu_name: &str) -> Result<(), rusqlite::Error> {
        let conn = self.lock_conn()?;
        conn.execute(
            "INSERT INTO vm_gpu_bindings (vm_id, gpu_name) VALUES (?1, ?2)
             ON CONFLICT(vm_id) DO UPDATE SET gpu_name = excluded.gpu_name",
            params![vm_id, gpu_name],
        )?;
        Ok(())
    }

    pub fn get_vm_gpu(&self, vm_id: &str) -> Result<Option<String>, rusqlite::Error> {
        let conn = self.lock_conn()?;
        let mut stmt = conn.prepare("SELECT gpu_name FROM vm_gpu_bindings WHERE vm_id = ?1")?;
        let mut rows = stmt.query_map(params![vm_id], |row| row.get::<_, String>(0))?;
        rows.next().transpose()
    }

    pub fn clear_vm_gpu(&self, vm_id: &str) -> Result<(), rusqlite::Error> {
        let conn = self.lock_conn()?;
        conn.execute(
            "DELETE FROM vm_gpu_bindings WHERE vm_id = ?1",
            params![vm_id],
        )?;
        Ok(())
    }

    pub fn replace_node_gpus(
        &self,
        node_id: &str,
        gpus: &[NodeGpuRow],
    ) -> Result<(), rusqlite::Error> {
        let mut conn = self.lock_conn()?;
        let tx = conn.transaction()?;
        tx.execute("DELETE FROM node_gpus WHERE node_id = ?1", params![node_id])?;
        for gpu in gpus {
            let kind = if gpu.kind.is_empty() {
                "gpu".to_string()
            } else {
                gpu.kind.clone()
            };
            tx.execute(
                "INSERT INTO node_gpus (node_id, name, family, model, address, pci_devices, iommu_group, assignable, blocked_reason, kind, role, class_code, characteristics, driver)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14)",
                params![
                    node_id,
                    gpu.name,
                    gpu.family,
                    gpu.model,
                    gpu.address,
                    gpu.pci_devices,
                    gpu.iommu_group,
                    gpu.assignable as i32,
                    gpu.blocked_reason,
                    kind,
                    gpu.role,
                    gpu.class_code,
                    gpu.characteristics,
                    gpu.driver,
                ],
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    pub fn list_node_gpus(
        &self,
        node_id: Option<&str>,
    ) -> Result<Vec<NodeGpuRow>, rusqlite::Error> {
        let conn = self.lock_conn()?;
        let mut stmt = conn.prepare(
            "SELECT node_id, name, family, model, address, pci_devices, iommu_group, assignable, blocked_reason, kind, role, class_code, characteristics, driver
             FROM node_gpus
             WHERE (?1 = '' OR node_id = ?1)
             ORDER BY node_id, address",
        )?;
        let filter = node_id.unwrap_or("");
        let rows = stmt.query_map(params![filter], |row| {
            Ok(NodeGpuRow {
                node_id: row.get(0)?,
                name: row.get(1)?,
                family: row.get(2)?,
                model: row.get(3)?,
                address: row.get(4)?,
                pci_devices: row.get(5)?,
                iommu_group: row.get(6)?,
                assignable: row.get::<_, i32>(7)? != 0,
                blocked_reason: row.get(8)?,
                kind: row.get(9)?,
                role: row.get(10)?,
                class_code: row.get(11)?,
                characteristics: row.get(12)?,
                driver: row.get(13)?,
            })
        })?;
        rows.collect()
    }

    pub fn set_vm_node(&self, vm_id: &str, node_id: &str) -> Result<bool, rusqlite::Error> {
        let conn = self.lock_conn()?;
        let rows = conn.execute(
            "UPDATE vms SET node_id = ?2 WHERE id = ?1",
            params![vm_id, node_id],
        )?;
        Ok(rows > 0)
    }

    pub fn update_vm_spec(
        &self,
        id_or_name: &str,
        cpu: Option<i32>,
        memory_bytes: Option<i64>,
    ) -> Result<bool, rusqlite::Error> {
        let conn = self.lock_conn()?;
        let mut parts = Vec::new();
        let mut values: Vec<Box<dyn rusqlite::types::ToSql>> = Vec::new();
        if let Some(c) = cpu {
            parts.push("cpu = ?");
            values.push(Box::new(c));
        }
        if let Some(m) = memory_bytes {
            parts.push("memory_bytes = ?");
            values.push(Box::new(m));
        }
        if parts.is_empty() {
            return Ok(false);
        }
        values.push(Box::new(id_or_name.to_string()));
        let set_clause = parts
            .iter()
            .enumerate()
            .map(|(i, p)| p.replace('?', &format!("?{}", i + 1)))
            .collect::<Vec<_>>()
            .join(", ");
        let id_param = values.len();
        let sql = format!(
            "UPDATE vms SET {} WHERE id = ?{} OR name = ?{}",
            set_clause, id_param, id_param
        );
        let refs: Vec<&dyn rusqlite::types::ToSql> = values.iter().map(|v| v.as_ref()).collect();
        let rows = conn.execute(&sql, refs.as_slice())?;
        Ok(rows > 0)
    }

    pub fn upsert_node_labels(
        &self,
        node_id: &str,
        labels: &[String],
    ) -> Result<(), rusqlite::Error> {
        let conn = self.lock_conn()?;
        conn.execute(
            "DELETE FROM node_labels WHERE node_id = ?1",
            params![node_id],
        )?;
        for label in labels {
            conn.execute(
                "INSERT INTO node_labels (node_id, label) VALUES (?1, ?2)",
                params![node_id, label],
            )?;
        }
        Ok(())
    }

    pub fn get_node_labels(&self, node_id: &str) -> Result<Vec<String>, rusqlite::Error> {
        let conn = self.lock_conn()?;
        let mut stmt = conn.prepare("SELECT label FROM node_labels WHERE node_id = ?1")?;
        let rows = stmt.query_map(params![node_id], |row| row.get::<_, String>(0))?;
        rows.collect()
    }

    pub fn get_all_node_labels(&self) -> Result<Vec<(String, String)>, rusqlite::Error> {
        let conn = self.lock_conn()?;
        let mut stmt = conn.prepare("SELECT node_id, label FROM node_labels ORDER BY node_id")?;
        let rows = stmt.query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })?;
        rows.collect()
    }

    pub fn insert_ssh_key(&self, name: &str, public_key: &str) -> Result<(), rusqlite::Error> {
        let conn = self.lock_conn()?;
        conn.execute(
            "INSERT INTO ssh_keys (name, public_key) VALUES (?1, ?2)",
            params![name, public_key],
        )?;
        Ok(())
    }

    pub fn upsert_ssh_key(&self, name: &str, public_key: &str) -> Result<(), rusqlite::Error> {
        let conn = self.lock_conn()?;
        conn.execute(
            "INSERT INTO ssh_keys (name, public_key) VALUES (?1, ?2)
             ON CONFLICT(name) DO UPDATE SET public_key = excluded.public_key",
            params![name, public_key],
        )?;
        Ok(())
    }

    pub fn get_ssh_key(
        &self,
        name: &str,
    ) -> Result<Option<(String, String, String)>, rusqlite::Error> {
        let conn = self.lock_conn()?;
        let mut stmt =
            conn.prepare("SELECT name, public_key, created_at FROM ssh_keys WHERE name = ?1")?;
        let mut rows = stmt.query_map(params![name], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
            ))
        })?;
        rows.next().transpose()
    }

    pub fn list_ssh_keys(&self) -> Result<Vec<(String, String, String)>, rusqlite::Error> {
        let conn = self.lock_conn()?;
        let mut stmt =
            conn.prepare("SELECT name, public_key, created_at FROM ssh_keys ORDER BY name")?;
        let rows = stmt.query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
            ))
        })?;
        rows.collect()
    }

    pub fn delete_ssh_key(&self, name: &str) -> Result<bool, rusqlite::Error> {
        let conn = self.lock_conn()?;
        conn.execute("DELETE FROM vm_ssh_keys WHERE key_name = ?1", params![name])?;
        let rows = conn.execute("DELETE FROM ssh_keys WHERE name = ?1", params![name])?;
        Ok(rows > 0)
    }

    fn unix_now_secs() -> i64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0)
    }

    pub fn count_operators(&self) -> Result<i64, rusqlite::Error> {
        let conn = self.lock_conn()?;
        conn.query_row("SELECT COUNT(*) FROM operators", [], |row| row.get(0))
    }

    pub fn create_operator(&self, name: &str) -> Result<(), rusqlite::Error> {
        let conn = self.lock_conn()?;
        let now = Self::unix_now_secs();
        conn.execute(
            "INSERT INTO operators (name, cert_serial, created_at, updated_at) VALUES (?1, '', ?2, ?2)",
            params![name, now],
        )?;
        Ok(())
    }

    /// Upsert operator row (replication / materialization).
    pub fn upsert_operator_row(&self, row: &OperatorRow) -> Result<(), rusqlite::Error> {
        let conn = self.lock_conn()?;
        conn.execute(
            "INSERT INTO operators (name, cert_serial, created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT(name) DO UPDATE SET
               cert_serial = excluded.cert_serial,
               updated_at = excluded.updated_at",
            params![row.name, row.cert_serial, row.created_at, row.updated_at],
        )?;
        Ok(())
    }

    pub fn delete_operator(&self, name: &str) -> Result<bool, rusqlite::Error> {
        let conn = self.lock_conn()?;
        let n = conn.execute("DELETE FROM operators WHERE name = ?1", params![name])?;
        Ok(n > 0)
    }

    pub fn get_operator_row(&self, name: &str) -> Result<Option<OperatorRow>, rusqlite::Error> {
        let conn = self.lock_conn()?;
        let mut stmt = conn.prepare(
            "SELECT name, cert_serial, created_at, updated_at FROM operators WHERE name = ?1",
        )?;
        let mut rows = stmt.query_map(params![name], |row| {
            Ok(OperatorRow {
                name: row.get(0)?,
                cert_serial: row.get(1)?,
                created_at: row.get(2)?,
                updated_at: row.get(3)?,
            })
        })?;
        rows.next().transpose()
    }

    pub fn list_operator_rows(&self) -> Result<Vec<OperatorRow>, rusqlite::Error> {
        let conn = self.lock_conn()?;
        let mut stmt = conn.prepare(
            "SELECT name, cert_serial, created_at, updated_at FROM operators ORDER BY name ASC",
        )?;
        let rows = stmt.query_map([], |row| {
            Ok(OperatorRow {
                name: row.get(0)?,
                cert_serial: row.get(1)?,
                created_at: row.get(2)?,
                updated_at: row.get(3)?,
            })
        })?;
        rows.collect()
    }

    pub fn list_operator_role_strings(
        &self,
        operator_name: &str,
    ) -> Result<Vec<String>, rusqlite::Error> {
        let conn = self.lock_conn()?;
        let mut stmt = conn.prepare(
            "SELECT role FROM operator_roles WHERE operator_name = ?1 ORDER BY role ASC",
        )?;
        let rows = stmt.query_map(params![operator_name], |row| row.get::<_, String>(0))?;
        rows.collect()
    }

    pub fn grant_operator_role_str(
        &self,
        operator_name: &str,
        role: &str,
    ) -> Result<(), rusqlite::Error> {
        let conn = self.lock_conn()?;
        let now = Self::unix_now_secs();
        conn.execute(
            "INSERT INTO operator_roles (operator_name, role, granted_at) VALUES (?1, ?2, ?3)
             ON CONFLICT(operator_name, role) DO UPDATE SET granted_at = excluded.granted_at",
            params![operator_name, role, now],
        )?;
        Ok(())
    }

    pub fn revoke_operator_role_str(
        &self,
        operator_name: &str,
        role: &str,
    ) -> Result<bool, rusqlite::Error> {
        let conn = self.lock_conn()?;
        let n = conn.execute(
            "DELETE FROM operator_roles WHERE operator_name = ?1 AND role = ?2",
            params![operator_name, role],
        )?;
        Ok(n > 0)
    }

    pub fn touch_operator_updated(&self, name: &str) -> Result<(), rusqlite::Error> {
        let conn = self.lock_conn()?;
        let now = Self::unix_now_secs();
        conn.execute(
            "UPDATE operators SET updated_at = ?2 WHERE name = ?1",
            params![name, now],
        )?;
        Ok(())
    }

    pub fn set_operator_cert_serial(
        &self,
        name: &str,
        serial: &str,
    ) -> Result<(), rusqlite::Error> {
        let conn = self.lock_conn()?;
        let now = Self::unix_now_secs();
        conn.execute(
            "UPDATE operators SET cert_serial = ?2, updated_at = ?3 WHERE name = ?1",
            params![name, serial, now],
        )?;
        Ok(())
    }

    pub fn associate_vm_ssh_keys(
        &self,
        vm_id: &str,
        key_names: &[String],
    ) -> Result<(), rusqlite::Error> {
        let conn = self.lock_conn()?;
        for key_name in key_names {
            conn.execute(
                "INSERT OR IGNORE INTO vm_ssh_keys (vm_id, key_name) VALUES (?1, ?2)",
                params![vm_id, key_name],
            )?;
        }
        Ok(())
    }

    pub fn get_vm_ssh_keys(&self, vm_id: &str) -> Result<Vec<String>, rusqlite::Error> {
        let conn = self.lock_conn()?;
        let mut stmt = conn.prepare(
            "SELECT sk.public_key FROM vm_ssh_keys vsk JOIN ssh_keys sk ON vsk.key_name = sk.name WHERE vsk.vm_id = ?1",
        )?;
        let rows = stmt.query_map(params![vm_id], |row| row.get::<_, String>(0))?;
        rows.collect()
    }

    pub fn get_vm_ssh_key_names(&self, vm_id: &str) -> Result<Vec<String>, rusqlite::Error> {
        let conn = self.lock_conn()?;
        let mut stmt = conn.prepare("SELECT key_name FROM vm_ssh_keys WHERE vm_id = ?1")?;
        let rows = stmt.query_map(params![vm_id], |row| row.get::<_, String>(0))?;
        rows.collect()
    }

    /// Remove a node row and its labels.
    ///
    /// VMs, networks, and workloads reference `nodes(id)` without cascade, so
    /// the caller must refuse while any of those rows remain.
    pub fn delete_node(&self, node_id: &str) -> Result<bool, rusqlite::Error> {
        let mut conn = self.lock_conn()?;
        let tx = conn.transaction()?;
        tx.execute(
            "DELETE FROM node_labels WHERE node_id = ?1",
            params![node_id],
        )?;
        let rows = tx.execute("DELETE FROM nodes WHERE id = ?1", params![node_id])?;
        tx.commit()?;
        Ok(rows > 0)
    }

    pub fn count_workloads_for_node(&self, node_id: &str) -> Result<i64, rusqlite::Error> {
        let conn = self.lock_conn()?;
        conn.query_row(
            "SELECT COUNT(*) FROM workloads WHERE node_id = ?1",
            params![node_id],
            |row| row.get(0),
        )
    }

    pub fn update_node_status(&self, node_id: &str, status: &str) -> Result<bool, rusqlite::Error> {
        let conn = self.lock_conn()?;
        let rows = conn.execute(
            "UPDATE nodes SET status = ?1 WHERE id = ?2",
            params![status, node_id],
        )?;
        Ok(rows > 0)
    }

    pub fn get_stale_nodes(&self, timeout_seconds: i64) -> Result<Vec<NodeRow>, rusqlite::Error> {
        let conn = self.lock_conn()?;
        let mut stmt = conn.prepare(
            "SELECT id, hostname, address, cpu_cores, memory_bytes, status, last_heartbeat, gateway_interface, cpu_used, memory_used, storage_backend, disable_vxlan FROM nodes WHERE status = 'ready' AND last_heartbeat != '' AND (julianday('now') - julianday(last_heartbeat)) * 86400 > ?1",
        )?;
        let rows = stmt.query_map(params![timeout_seconds], row_to_node)?;
        rows.collect()
    }

    pub fn count_nodes_by_approval(&self) -> Result<(i32, i32, i32), rusqlite::Error> {
        let conn = self.lock_conn()?;
        let mut stmt =
            conn.prepare("SELECT approval_status, COUNT(*) FROM nodes GROUP BY approval_status")?;
        let rows = stmt.query_map([], |row| {
            let status: String = row.get(0)?;
            let count: i32 = row.get(1)?;
            Ok((status, count))
        })?;
        let (mut approved, mut pending, mut rejected) = (0, 0, 0);
        for row in rows {
            let (status, count) = row?;
            match status.as_str() {
                "approved" => approved = count,
                "pending" => pending = count,
                "rejected" => rejected = count,
                _ => {}
            }
        }
        Ok((approved, pending, rejected))
    }

    pub fn count_vms_by_auto_start(&self) -> Result<(i32, i32), rusqlite::Error> {
        let conn = self.lock_conn()?;
        let total: i32 = conn.query_row("SELECT COUNT(*) FROM vms", [], |row| row.get(0))?;
        let running: i32 =
            conn.query_row("SELECT COUNT(*) FROM vms WHERE auto_start = 1", [], |row| {
                row.get(0)
            })?;
        Ok((total, running))
    }

    pub fn count_networks_by_type(&self) -> Result<(i32, i32, i32), rusqlite::Error> {
        let conn = self.lock_conn()?;
        let mut stmt =
            conn.prepare("SELECT network_type, COUNT(*) FROM networks GROUP BY network_type")?;
        let rows = stmt.query_map([], |row| {
            let ntype: String = row.get(0)?;
            let count: i32 = row.get(1)?;
            Ok((ntype, count))
        })?;
        let (mut nat, mut bridge, mut vxlan) = (0, 0, 0);
        for row in rows {
            let (ntype, count) = row?;
            match ntype.as_str() {
                "nat" => nat = count,
                "bridge" => bridge = count,
                "vxlan" => vxlan = count,
                _ => {}
            }
        }
        Ok((nat, bridge, vxlan))
    }

    pub fn count_nodes_cert_expiry(&self) -> Result<(i32, i32), rusqlite::Error> {
        let conn = self.lock_conn()?;
        let expiring: i32 = conn.query_row(
            "SELECT COUNT(*) FROM nodes WHERE cert_expiry_days > 0 AND cert_expiry_days <= 30",
            [],
            |row| row.get(0),
        )?;
        let unknown: i32 = conn.query_row(
            "SELECT COUNT(*) FROM nodes WHERE cert_expiry_days < 0",
            [],
            |row| row.get(0),
        )?;
        Ok((expiring, unknown))
    }

    pub fn count_nodes_luks_method(&self) -> Result<(i32, i32, i32), rusqlite::Error> {
        let conn = self.lock_conn()?;
        let tpm2: i32 = conn.query_row(
            "SELECT COUNT(*) FROM nodes WHERE luks_method = 'tpm2'",
            [],
            |row| row.get(0),
        )?;
        let keyfile: i32 = conn.query_row(
            "SELECT COUNT(*) FROM nodes WHERE luks_method = 'key-file'",
            [],
            |row| row.get(0),
        )?;
        let unknown: i32 = conn.query_row(
            "SELECT COUNT(*) FROM nodes WHERE luks_method = '' OR luks_method IS NULL",
            [],
            |row| row.get(0),
        )?;
        Ok((tpm2, keyfile, unknown))
    }

    // --- Certificate inventory (issued_certificates) -----------------------

    /// Record a newly signed certificate. Re-recording the same serial is a
    /// no-op on the revocation columns so a replayed insert cannot resurrect a
    /// revoked serial as active.
    pub fn record_issued_certificate(&self, row: &IssuedCertRow) -> Result<(), rusqlite::Error> {
        let conn = self.lock_conn()?;
        conn.execute(
            "INSERT INTO issued_certificates
                (serial_hex, subject_cn, identity_kind, node_id, issuer_cn,
                 fingerprint_sha256, not_before, not_after, issued_at, status,
                 revocation_reason, revoked_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)
             ON CONFLICT(serial_hex) DO UPDATE SET
                subject_cn=excluded.subject_cn,
                identity_kind=excluded.identity_kind,
                node_id=excluded.node_id,
                issuer_cn=excluded.issuer_cn,
                fingerprint_sha256=excluded.fingerprint_sha256,
                not_before=excluded.not_before,
                not_after=excluded.not_after",
            params![
                row.serial_hex,
                row.subject_cn,
                row.identity_kind,
                row.node_id,
                row.issuer_cn,
                row.fingerprint_sha256,
                row.not_before,
                row.not_after,
                row.issued_at,
                row.status,
                row.revocation_reason,
                row.revoked_at,
            ],
        )?;
        Ok(())
    }

    /// Mark every `active` certificate for `subject_cn` (except `keep_serial`)
    /// as `rotated`. Revoked rows are left alone.
    pub fn mark_superseded_certificates(
        &self,
        subject_cn: &str,
        keep_serial: &str,
    ) -> Result<usize, rusqlite::Error> {
        let conn = self.lock_conn()?;
        conn.execute(
            "UPDATE issued_certificates SET status=?3
             WHERE subject_cn=?1 AND serial_hex<>?2 AND status=?4",
            params![
                subject_cn,
                keep_serial,
                CERT_STATUS_ROTATED,
                CERT_STATUS_ACTIVE
            ],
        )
    }

    pub fn get_issued_certificate(
        &self,
        serial_hex: &str,
    ) -> Result<Option<IssuedCertRow>, rusqlite::Error> {
        let conn = self.lock_conn()?;
        let mut stmt = conn.prepare(&format!("{ISSUED_CERT_SELECT} WHERE serial_hex=?1"))?;
        let mut rows = stmt.query_map(params![serial_hex], map_issued_cert_row)?;
        match rows.next() {
            Some(row) => Ok(Some(row?)),
            None => Ok(None),
        }
    }

    /// Inventory rows ordered by soonest expiry first.
    ///
    /// `status` filters on the status column when non-empty; `node_id` filters
    /// on node when non-empty; `expiring_before` (RFC3339) keeps only rows with
    /// `not_after` strictly before it when non-empty.
    pub fn list_issued_certificates(
        &self,
        status: &str,
        node_id: &str,
        expiring_before: &str,
    ) -> Result<Vec<IssuedCertRow>, rusqlite::Error> {
        let conn = self.lock_conn()?;
        let mut stmt = conn.prepare(&format!(
            "{ISSUED_CERT_SELECT}
             WHERE (?1 = '' OR status = ?1)
               AND (?2 = '' OR node_id = ?2)
               AND (?3 = '' OR not_after < ?3)
             ORDER BY not_after ASC, serial_hex ASC"
        ))?;
        let rows = stmt.query_map(
            params![status, node_id, expiring_before],
            map_issued_cert_row,
        )?;
        rows.collect()
    }

    /// Active, unexpired certificates whose `not_after` is at or before
    /// `threshold` (RFC3339) — the rotation reconciler's work queue.
    pub fn list_certificates_needing_rotation(
        &self,
        threshold: &str,
        now: &str,
    ) -> Result<Vec<IssuedCertRow>, rusqlite::Error> {
        let conn = self.lock_conn()?;
        let mut stmt = conn.prepare(&format!(
            "{ISSUED_CERT_SELECT}
             WHERE status = ?3 AND not_after <= ?1 AND not_after > ?2
             ORDER BY not_after ASC, serial_hex ASC"
        ))?;
        let rows = stmt.query_map(
            params![threshold, now, CERT_STATUS_ACTIVE],
            map_issued_cert_row,
        )?;
        rows.collect()
    }

    /// Newest active certificate for a node, if the node has one on record.
    pub fn get_active_certificate_for_node(
        &self,
        node_id: &str,
    ) -> Result<Option<IssuedCertRow>, rusqlite::Error> {
        let conn = self.lock_conn()?;
        let mut stmt = conn.prepare(&format!(
            "{ISSUED_CERT_SELECT}
             WHERE node_id=?1 AND status=?2
             ORDER BY not_after DESC, serial_hex ASC LIMIT 1"
        ))?;
        let mut rows = stmt.query_map(params![node_id, CERT_STATUS_ACTIVE], map_issued_cert_row)?;
        match rows.next() {
            Some(row) => Ok(Some(row?)),
            None => Ok(None),
        }
    }

    /// Revoke by serial. Returns the updated row, or `None` when the serial is
    /// unknown. Already-revoked serials keep their original reason and time so
    /// the CRL entry stays stable.
    pub fn revoke_certificate_by_serial(
        &self,
        serial_hex: &str,
        reason: i32,
        revoked_at: &str,
    ) -> Result<Option<IssuedCertRow>, rusqlite::Error> {
        {
            let conn = self.lock_conn()?;
            conn.execute(
                "UPDATE issued_certificates
                 SET status=?2, revocation_reason=?3, revoked_at=?4
                 WHERE serial_hex=?1 AND status<>?2",
                params![serial_hex, CERT_STATUS_REVOKED, reason, revoked_at],
            )?;
        }
        self.get_issued_certificate(serial_hex)
    }

    /// Serials to revoke for an identity: every non-revoked row matching
    /// `subject_cn` (when non-empty) or `node_id` (when non-empty).
    pub fn find_revocable_serials(
        &self,
        subject_cn: &str,
        node_id: &str,
    ) -> Result<Vec<String>, rusqlite::Error> {
        let conn = self.lock_conn()?;
        let mut stmt = conn.prepare(
            "SELECT serial_hex FROM issued_certificates
             WHERE status <> ?3
               AND ((?1 <> '' AND subject_cn = ?1) OR (?2 <> '' AND node_id = ?2))
             ORDER BY not_after DESC, serial_hex ASC",
        )?;
        let rows = stmt.query_map(params![subject_cn, node_id, CERT_STATUS_REVOKED], |r| {
            r.get::<_, String>(0)
        })?;
        rows.collect()
    }

    /// `(serial_hex, revocation_reason, revoked_at)` for every revoked row —
    /// the CRL contents.
    pub fn list_revoked_certificates(&self) -> Result<Vec<(String, i32, String)>, rusqlite::Error> {
        let conn = self.lock_conn()?;
        let mut stmt = conn.prepare(
            "SELECT serial_hex, revocation_reason, revoked_at
             FROM issued_certificates WHERE status = ?1
             ORDER BY serial_hex ASC",
        )?;
        let rows = stmt.query_map(params![CERT_STATUS_REVOKED], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?))
        })?;
        rows.collect()
    }

    /// Just the revoked serial set, for the hot enforcement path.
    pub fn revoked_serial_set(&self) -> Result<std::collections::HashSet<String>, rusqlite::Error> {
        let conn = self.lock_conn()?;
        let mut stmt =
            conn.prepare("SELECT serial_hex FROM issued_certificates WHERE status = ?1")?;
        let rows = stmt.query_map(params![CERT_STATUS_REVOKED], |r| r.get::<_, String>(0))?;
        rows.collect()
    }

    /// `(active, rotated, revoked, expired, expiring_soon)` inventory counts.
    /// `expired` counts non-revoked rows already past `not_after`; a row is
    /// counted once, with `expired` taking precedence over `active`.
    pub fn count_certificates(
        &self,
        now: &str,
        warn_threshold: &str,
    ) -> Result<(i32, i32, i32, i32, i32), rusqlite::Error> {
        let conn = self.lock_conn()?;
        let count = |sql: &str, p: &[&dyn rusqlite::ToSql]| -> Result<i32, rusqlite::Error> {
            conn.query_row(sql, p, |row| row.get(0))
        };
        let active = count(
            "SELECT COUNT(*) FROM issued_certificates WHERE status=?1 AND not_after > ?2",
            &[&CERT_STATUS_ACTIVE, &now],
        )?;
        let rotated = count(
            "SELECT COUNT(*) FROM issued_certificates WHERE status=?1 AND not_after > ?2",
            &[&CERT_STATUS_ROTATED, &now],
        )?;
        let revoked = count(
            "SELECT COUNT(*) FROM issued_certificates WHERE status=?1",
            &[&CERT_STATUS_REVOKED],
        )?;
        let expired = count(
            "SELECT COUNT(*) FROM issued_certificates WHERE status<>?1 AND not_after <= ?2",
            &[&CERT_STATUS_REVOKED, &now],
        )?;
        let expiring_soon = count(
            "SELECT COUNT(*) FROM issued_certificates
             WHERE status=?1 AND not_after > ?2 AND not_after <= ?3",
            &[&CERT_STATUS_ACTIVE, &now, &warn_threshold],
        )?;
        Ok((active, rotated, revoked, expired, expiring_soon))
    }

    // --- CRL state (crl_state, single row) ---------------------------------

    pub fn get_crl_state(&self) -> Result<Option<CrlStateRow>, rusqlite::Error> {
        let conn = self.lock_conn()?;
        let mut stmt = conn.prepare(
            "SELECT crl_number, this_update, next_update, crl_pem, crl_der,
                    revoked_count, issuer_fingerprint
             FROM crl_state WHERE id = 1",
        )?;
        let mut rows = stmt.query_map([], |r| {
            Ok(CrlStateRow {
                crl_number: r.get(0)?,
                this_update: r.get(1)?,
                next_update: r.get(2)?,
                crl_pem: r.get(3)?,
                crl_der: r.get(4)?,
                revoked_count: r.get(5)?,
                issuer_fingerprint: r.get(6)?,
            })
        })?;
        match rows.next() {
            Some(row) => Ok(Some(row?)),
            None => Ok(None),
        }
    }

    pub fn put_crl_state(&self, row: &CrlStateRow) -> Result<(), rusqlite::Error> {
        let conn = self.lock_conn()?;
        conn.execute(
            "INSERT INTO crl_state
                (id, crl_number, this_update, next_update, crl_pem, crl_der,
                 revoked_count, issuer_fingerprint)
             VALUES (1, ?1, ?2, ?3, ?4, ?5, ?6, ?7)
             ON CONFLICT(id) DO UPDATE SET
                crl_number=excluded.crl_number,
                this_update=excluded.this_update,
                next_update=excluded.next_update,
                crl_pem=excluded.crl_pem,
                crl_der=excluded.crl_der,
                revoked_count=excluded.revoked_count,
                issuer_fingerprint=excluded.issuer_fingerprint",
            params![
                row.crl_number,
                row.this_update,
                row.next_update,
                row.crl_pem,
                row.crl_der,
                row.revoked_count,
                row.issuer_fingerprint,
            ],
        )?;
        Ok(())
    }
}

const ISSUED_CERT_SELECT: &str = "SELECT serial_hex, subject_cn, identity_kind, node_id, issuer_cn,
            fingerprint_sha256, not_before, not_after, issued_at, status,
            revocation_reason, revoked_at
     FROM issued_certificates";

fn map_issued_cert_row(row: &rusqlite::Row<'_>) -> Result<IssuedCertRow, rusqlite::Error> {
    Ok(IssuedCertRow {
        serial_hex: row.get(0)?,
        subject_cn: row.get(1)?,
        identity_kind: row.get(2)?,
        node_id: row.get(3)?,
        issuer_cn: row.get(4)?,
        fingerprint_sha256: row.get(5)?,
        not_before: row.get(6)?,
        not_after: row.get(7)?,
        issued_at: row.get(8)?,
        status: row.get(9)?,
        revocation_reason: row.get(10)?,
        revoked_at: row.get(11)?,
    })
}

fn row_to_node(row: &rusqlite::Row) -> Result<NodeRow, rusqlite::Error> {
    let disable_vxlan_int: i32 = row.get(11)?;
    Ok(NodeRow {
        id: row.get(0)?,
        hostname: row.get(1)?,
        address: row.get(2)?,
        cpu_cores: row.get(3)?,
        memory_bytes: row.get(4)?,
        status: row.get(5)?,
        last_heartbeat: row.get(6)?,
        gateway_interface: row.get(7)?,
        cpu_used: row.get(8)?,
        memory_used: row.get(9)?,
        storage_backend: row.get(10)?,
        disable_vxlan: disable_vxlan_int != 0,
        approval_status: row.get(12)?,
        cert_expiry_days: row.get(13)?,
        luks_method: row.get(14)?,
        dc_id: row.get(15)?,
    })
}

fn row_to_vm(row: &rusqlite::Row) -> Result<VmRow, rusqlite::Error> {
    let image_path: String = row.get(4)?;
    let image_url: String = row.get(5)?;
    let image_format: String = row.get(7)?;
    Ok(VmRow {
        id: row.get(0)?,
        name: row.get(1)?,
        cpu: row.get(2)?,
        memory_bytes: row.get(3)?,
        image_path: image_path.clone(),
        image_url: image_url.clone(),
        image_sha256: row.get(6)?,
        image_format: normalize_image_format(&image_format, &image_path, &image_url),
        image_size: row.get(8)?,
        network: row.get(9)?,
        auto_start: row.get::<_, i32>(10)? != 0,
        node_id: row.get(11)?,
        created_at: row.get(12)?,
        runtime_state: row.get(13)?,
        cloud_init_user_data: row.get(14)?,
        storage_backend: row.get(15)?,
        storage_size_bytes: row.get(16)?,
        vm_ip: row.get(17)?,
        pci_devices: row.get(18)?,
    })
}

fn row_to_network_policy(row: &rusqlite::Row) -> Result<NetworkPolicyRow, rusqlite::Error> {
    Ok(NetworkPolicyRow {
        node_id: row.get(0)?,
        name: row.get(1)?,
        east_west: row.get::<_, i32>(2)? != 0,
        ipv6_prefix: row.get(3)?,
        ipv6_gateway: row.get(4)?,
        ipv6_next: row.get(5)?,
    })
}

fn row_to_vm_ipv6(row: &rusqlite::Row) -> Result<VmIpv6Row, rusqlite::Error> {
    Ok(VmIpv6Row {
        vm_id: row.get(0)?,
        position: row.get(1)?,
        address: row.get(2)?,
    })
}

fn take_released_vxlan_ip(
    conn: &rusqlite::Connection,
    network_name: &str,
) -> Result<Option<String>, rusqlite::Error> {
    let ip = match conn.query_row(
        "SELECT ip FROM vxlan_released_ips
         WHERE network = ?1
           AND ip NOT IN (SELECT vm_ip FROM vms WHERE vm_ip != '')
           AND ip NOT IN (SELECT ip_address FROM vm_nics WHERE ip_address != '')
         ORDER BY ip LIMIT 1",
        params![network_name],
        |row| row.get::<_, String>(0),
    ) {
        Ok(ip) => ip,
        Err(rusqlite::Error::QueryReturnedNoRows) => return Ok(None),
        Err(e) => return Err(e),
    };
    conn.execute(
        "DELETE FROM vxlan_released_ips WHERE network = ?1 AND ip = ?2",
        params![network_name, ip],
    )?;
    Ok(Some(ip))
}

fn take_released_ipv6(
    conn: &rusqlite::Connection,
    scope: &str,
) -> Result<Option<String>, rusqlite::Error> {
    let address = match conn.query_row(
        "SELECT address FROM ipv6_released
         WHERE scope = ?1
           AND address NOT IN (SELECT address FROM vm_ipv6)
         ORDER BY address LIMIT 1",
        params![scope],
        |row| row.get::<_, String>(0),
    ) {
        Ok(address) => address,
        Err(rusqlite::Error::QueryReturnedNoRows) => return Ok(None),
        Err(e) => return Err(e),
    };
    conn.execute(
        "DELETE FROM ipv6_released WHERE scope = ?1 AND address = ?2",
        params![scope, address],
    )?;
    Ok(Some(address))
}

fn release_vm_addresses(
    conn: &rusqlite::Connection,
    vm_id: &str,
    node_id: &str,
    primary_network: &str,
    primary_ip: &str,
) -> Result<(), rusqlite::Error> {
    release_vxlan_ip(conn, node_id, primary_network, primary_ip)?;
    let extras: Vec<(i32, String, String)> = {
        let mut stmt =
            conn.prepare("SELECT position, network, ip_address FROM vm_nics WHERE vm_id = ?1")?;
        let rows = stmt.query_map(params![vm_id], |row| {
            Ok((
                row.get::<_, i32>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
            ))
        })?;
        rows.collect::<Result<Vec<_>, _>>()?
    };
    for (_position, network, ip) in &extras {
        release_vxlan_ip(conn, node_id, network, ip)?;
    }
    let ipv6: Vec<(i32, String)> = {
        let mut stmt = conn.prepare("SELECT position, address FROM vm_ipv6 WHERE vm_id = ?1")?;
        let rows = stmt.query_map(params![vm_id], |row| {
            Ok((row.get::<_, i32>(0)?, row.get::<_, String>(1)?))
        })?;
        rows.collect::<Result<Vec<_>, _>>()?
    };
    for (position, address) in ipv6 {
        let network = if position == 0 {
            primary_network.to_string()
        } else {
            extras
                .iter()
                .find(|(pos, _, _)| *pos == position)
                .map(|(_, network, _)| network.clone())
                .unwrap_or_else(|| primary_network.to_string())
        };
        release_ipv6(conn, node_id, &network, &address)?;
    }
    Ok(())
}

fn release_vxlan_ip(
    conn: &rusqlite::Connection,
    node_id: &str,
    network: &str,
    ip: &str,
) -> Result<(), rusqlite::Error> {
    if ip.trim().is_empty() {
        return Ok(());
    }
    let meta = match conn.query_row(
        "SELECT network_type, gateway_ip FROM networks WHERE node_id = ?1 AND name = ?2",
        params![node_id, network],
        |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
    ) {
        Ok(meta) => meta,
        Err(rusqlite::Error::QueryReturnedNoRows) => return Ok(()),
        Err(e) => return Err(e),
    };
    if meta.0 != "vxlan" || ip == meta.1 {
        return Ok(());
    }
    conn.execute(
        "INSERT OR IGNORE INTO vxlan_released_ips (network, ip) VALUES (?1, ?2)",
        params![network, ip],
    )?;
    Ok(())
}

fn release_ipv6(
    conn: &rusqlite::Connection,
    node_id: &str,
    network: &str,
    address: &str,
) -> Result<(), rusqlite::Error> {
    if address.trim().is_empty() {
        return Ok(());
    }
    let policy = match conn.query_row(
        "SELECT ipv6_gateway FROM network_policy WHERE node_id = ?1 AND name = ?2",
        params![node_id, network],
        |row| row.get::<_, String>(0),
    ) {
        Ok(gateway) => gateway,
        Err(rusqlite::Error::QueryReturnedNoRows) => return Ok(()),
        Err(e) => return Err(e),
    };
    if address == policy {
        return Ok(());
    }
    let global = match conn.query_row(
        "SELECT network_type FROM networks WHERE node_id = ?1 AND name = ?2",
        params![node_id, network],
        |row| row.get::<_, String>(0),
    ) {
        Ok(kind) => kind == "vxlan",
        Err(rusqlite::Error::QueryReturnedNoRows) => false,
        Err(e) => return Err(e),
    };
    let scope = crate::net_policy::ipv6_scope(global, node_id, network);
    conn.execute(
        "INSERT OR IGNORE INTO ipv6_released (scope, address) VALUES (?1, ?2)",
        params![scope, address],
    )?;
    Ok(())
}

fn row_to_network(row: &rusqlite::Row) -> Result<NetworkRow, rusqlite::Error> {
    Ok(NetworkRow {
        name: row.get(0)?,
        external_ip: row.get(1)?,
        gateway_ip: row.get(2)?,
        internal_netmask: row.get(3)?,
        node_id: row.get(4)?,
        allowed_tcp_ports: row.get(5)?,
        allowed_udp_ports: row.get(6)?,
        vlan_id: row.get(7)?,
        network_type: row.get(8)?,
        enable_outbound_nat: row.get::<_, i32>(9)? != 0,
        vni: row.get(10)?,
        next_ip: row.get(11)?,
    })
}

fn row_to_postgresql(row: &rusqlite::Row) -> Result<PostgresqlRow, rusqlite::Error> {
    Ok(PostgresqlRow {
        name: row.get(0)?,
        database_name: row.get(1)?,
        package: row.get(2)?,
        port: row.get(3)?,
        node_id: row.get(4)?,
        created_at: row.get(5)?,
    })
}

fn row_to_workload(row: &rusqlite::Row) -> Result<WorkloadRow, rusqlite::Error> {
    Ok(WorkloadRow {
        id: row.get(0)?,
        name: row.get(1)?,
        kind: row.get(2)?,
        node_id: row.get(3)?,
        runtime_state: row.get(4)?,
        desired_state: row.get(5)?,
        vm_id: row.get(6)?,
        container_image: row.get(7)?,
        network: row.get(8)?,
        storage_backend: row.get(9)?,
        storage_size_bytes: row.get(10)?,
        created_at: row.get(11)?,
    })
}

fn map_audit_event_row(row: &rusqlite::Row<'_>) -> Result<AuditEventRow, rusqlite::Error> {
    Ok(AuditEventRow {
        id: row.get(0)?,
        actor: row.get(1)?,
        action: row.get(2)?,
        resource: row.get(3)?,
        created_at: row.get(4)?,
        detail: row.get(5)?,
    })
}

fn normalize_image_format(format: &str, image_path: &str, image_url: &str) -> String {
    let normalized = format.trim().to_ascii_lowercase();
    match normalized.as_str() {
        // Legacy rows can end up with "raw" as a default after migration.
        // If they come from URL-backed images and the path clearly looks qcow2, correct it.
        "raw"
            if !image_url.is_empty()
                && matches!(infer_image_format_from_path(image_path), "qcow2") =>
        {
            "qcow2".to_string()
        }
        "raw" | "qcow2" => normalized,
        _ => infer_image_format_from_path(image_path).to_string(),
    }
}

fn infer_image_format_from_path(path: &str) -> &'static str {
    let lower = path.to_ascii_lowercase();
    if lower.ends_with(".qcow2") || lower.ends_with(".qcow") {
        "qcow2"
    } else {
        "raw"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_node() -> NodeRow {
        NodeRow {
            id: "n1".to_string(),
            hostname: "n1".to_string(),
            address: "127.0.0.1:9091".to_string(),
            cpu_cores: 4,
            memory_bytes: 8 * 1024 * 1024 * 1024,
            status: "ready".to_string(),
            last_heartbeat: String::new(),
            gateway_interface: "eno1".to_string(),
            cpu_used: 0,
            memory_used: 0,
            storage_backend: "filesystem".to_string(),
            disable_vxlan: false,
            approval_status: "approved".to_string(),
            cert_expiry_days: -1,
            luks_method: String::new(),
            dc_id: "DC1".to_string(),
        }
    }

    fn test_vm(node_id: &str) -> VmRow {
        VmRow {
            id: "vm-1".to_string(),
            name: "web-1".to_string(),
            cpu: 2,
            memory_bytes: 2 * 1024 * 1024 * 1024,
            image_path: "/var/lib/kcore/images/web-1.raw".to_string(),
            image_url: String::new(),
            image_sha256: String::new(),
            image_format: "raw".to_string(),
            image_size: 8192,
            network: "default".to_string(),
            auto_start: true,
            node_id: node_id.to_string(),
            created_at: String::new(),
            runtime_state: "unknown".to_string(),
            cloud_init_user_data: String::new(),
            storage_backend: "filesystem".to_string(),
            storage_size_bytes: 0,
            vm_ip: String::new(),
            pci_devices: String::new(),
        }
    }

    #[test]
    fn vm_pci_devices_round_trip() {
        let db = Database::open(":memory:").expect("open db");
        let node = test_node();
        db.upsert_node(&node).expect("insert node");
        let mut vm = test_vm(&node.id);
        vm.pci_devices = "0000:03:00.0,0000:03:00.1".into();
        db.insert_vm(&vm).expect("insert vm");
        let got = db.get_vm("vm-1").expect("get vm").expect("vm");
        assert_eq!(got.pci_devices, "0000:03:00.0,0000:03:00.1");
    }

    fn test_workload(node_id: &str) -> WorkloadRow {
        WorkloadRow {
            id: "wl-1".to_string(),
            name: "workload-1".to_string(),
            kind: "container".to_string(),
            node_id: node_id.to_string(),
            runtime_state: "running".to_string(),
            desired_state: "running".to_string(),
            vm_id: String::new(),
            container_image: "nginx:alpine".to_string(),
            network: "default".to_string(),
            storage_backend: "filesystem".to_string(),
            storage_size_bytes: 1024,
            created_at: String::new(),
        }
    }

    #[test]
    fn open_rejects_path_with_dot_dot_segments() {
        for bad in [
            "../evil.db",
            "foo/../../etc/passwd",
            "/tmp/myapp/../../../../var/tmp/malicious_dir/db.sqlite",
            r"foo\..\..\secret.db",
            "file:../../../tmp/x.db",
        ] {
            let err = Database::open(bad).expect_err("path traversal should be rejected");
            let msg = format!("{err:#}");
            assert!(
                msg.contains("..") || msg.contains("parent directory"),
                "unexpected error for {bad:?}: {msg}"
            );
        }
    }

    #[test]
    fn open_creates_parent_for_safe_path_under_temp() {
        let unique = uuid::Uuid::new_v4();
        let root = std::env::temp_dir().join(format!("kcore-db-open-test-{unique}"));
        let path = root.join("nested/controller.sqlite");
        let path_str = path.to_str().expect("utf-8 temp path");
        let _ = std::fs::remove_dir_all(&root);

        let db = Database::open(path_str).expect("open db with mkdir");
        drop(db);

        assert!(path.is_file(), "database file should exist at {path_str}");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn set_vm_auto_start_updates_by_name() {
        let db = Database::open(":memory:").expect("open db");
        let node = test_node();
        db.upsert_node(&node).expect("insert node");
        db.insert_vm(&test_vm(&node.id)).expect("insert vm");

        let changed = db
            .set_vm_auto_start("web-1", false)
            .expect("update auto_start");
        assert!(changed);

        let updated = db.get_vm("vm-1").expect("get vm").expect("vm");
        assert!(!updated.auto_start);
    }

    #[test]
    fn set_vm_auto_start_if_current_skips_when_value_mismatch() {
        let db = Database::open(":memory:").expect("open db");
        let node = test_node();
        db.upsert_node(&node).expect("insert node");
        db.insert_vm(&test_vm(&node.id)).expect("insert vm");

        assert!(
            db.set_vm_auto_start_if_current("web-1", true, false)
                .expect("cas"),
            "expected update from true to false"
        );
        assert!(
            !db.set_vm_auto_start_if_current("web-1", true, true)
                .expect("cas skip"),
            "expected no update when expected_current does not match"
        );
        let v = db.get_vm("vm-1").expect("get vm").expect("vm");
        assert!(!v.auto_start);
    }

    #[test]
    fn migrate_infers_qcow2_for_legacy_url_rows() {
        let db = Database::open(":memory:").expect("open db");
        let node = test_node();
        db.upsert_node(&node).expect("insert node");

        db.insert_vm(&VmRow {
            id: "vm-qcow".to_string(),
            name: "vm-qcow".to_string(),
            cpu: 1,
            memory_bytes: 1024 * 1024 * 1024,
            image_path: "/var/lib/kcore/images/debian.qcow2".to_string(),
            image_url: "https://example.com/debian.qcow2".to_string(),
            image_sha256: "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
                .to_string(),
            image_format: "raw".to_string(),
            image_size: 4096,
            network: "default".to_string(),
            auto_start: true,
            node_id: node.id.clone(),
            created_at: String::new(),
            runtime_state: "unknown".to_string(),
            cloud_init_user_data: String::new(),
            storage_backend: "filesystem".to_string(),
            storage_size_bytes: 0,
            vm_ip: String::new(),
            pci_devices: String::new(),
        })
        .expect("insert qcow vm");

        let vm = db.get_vm("vm-qcow").expect("get vm").expect("vm exists");
        assert_eq!(vm.image_format, "qcow2");
    }

    #[test]
    fn network_roundtrip_works() {
        let db = Database::open(":memory:").expect("open db");
        let node = test_node();
        db.upsert_node(&node).expect("insert node");
        db.insert_network(&NetworkRow {
            name: "frontend".to_string(),
            external_ip: "203.0.113.10".to_string(),
            gateway_ip: "10.240.10.1".to_string(),
            internal_netmask: "255.255.255.0".to_string(),
            node_id: node.id.clone(),
            allowed_tcp_ports: String::new(),
            allowed_udp_ports: String::new(),
            vlan_id: 0,
            network_type: "nat".to_string(),
            enable_outbound_nat: true,
            vni: 0,
            next_ip: 2,
        })
        .expect("insert network");

        let got = db
            .get_network_for_node(&node.id, "frontend")
            .expect("get network")
            .expect("network exists");
        assert_eq!(got.gateway_ip, "10.240.10.1");

        let listed = db
            .list_networks_for_node(&node.id)
            .expect("list networks for node");
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].name, "frontend");
    }

    #[test]
    fn node_storage_backend_roundtrip_works() {
        let db = Database::open(":memory:").expect("open db");
        let mut node = test_node();
        node.storage_backend = "zfs".to_string();
        db.upsert_node(&node).expect("insert node");

        let got = db
            .get_node(&node.id)
            .expect("get node")
            .expect("node exists");
        assert_eq!(got.storage_backend, "zfs");
    }

    #[test]
    fn node_disable_vxlan_roundtrip() {
        let db = Database::open(":memory:").expect("open db");
        let mut node = test_node();
        node.disable_vxlan = true;
        db.upsert_node(&node).expect("insert node");

        let got = db
            .get_node(&node.id)
            .expect("get node")
            .expect("node exists");
        assert!(got.disable_vxlan);

        node.disable_vxlan = false;
        db.upsert_node(&node).expect("update node");
        let got2 = db
            .get_node(&node.id)
            .expect("get node")
            .expect("node exists");
        assert!(!got2.disable_vxlan);
    }

    #[test]
    fn node_approval_status_roundtrip() {
        let db = Database::open(":memory:").expect("open db");
        let mut node = test_node();
        node.approval_status = "pending".to_string();
        db.upsert_node(&node).expect("insert node");

        let got = db.get_node(&node.id).expect("get").expect("exists");
        assert_eq!(got.approval_status, "pending");

        db.set_node_approval(&node.id, "approved").expect("approve");
        let got2 = db.get_node(&node.id).expect("get").expect("exists");
        assert_eq!(got2.approval_status, "approved");

        db.set_node_approval(&node.id, "rejected").expect("reject");
        let got3 = db.get_node(&node.id).expect("get").expect("exists");
        assert_eq!(got3.approval_status, "rejected");
    }

    #[test]
    fn upsert_node_preserves_approval_status() {
        let db = Database::open(":memory:").expect("open db");
        let mut node = test_node();
        node.approval_status = "pending".to_string();
        db.upsert_node(&node).expect("insert");

        db.set_node_approval(&node.id, "approved").expect("approve");

        node.hostname = "updated-hostname".to_string();
        node.approval_status = "pending".to_string();
        db.upsert_node(&node).expect("upsert again");

        let got = db.get_node(&node.id).expect("get").expect("exists");
        assert_eq!(got.hostname, "updated-hostname");
        assert_eq!(
            got.approval_status, "approved",
            "upsert should not overwrite approval_status"
        );
    }

    #[test]
    fn heartbeat_updates_timestamp_but_preserves_status_for_non_approved_nodes() {
        let db = Database::open(":memory:").expect("open db");
        let mut node = test_node();
        node.approval_status = "pending".to_string();
        node.status = "pending".to_string();
        db.upsert_node(&node).expect("insert");

        let updated = db
            .update_heartbeat(&node.id, 1, 1000, -1, "")
            .expect("heartbeat");
        assert!(
            updated,
            "heartbeat should update a registered node regardless of approval"
        );

        let got = db.get_node(&node.id).expect("get").expect("exists");
        assert_eq!(
            got.status, "pending",
            "status should still be pending for non-approved"
        );
        assert!(
            !got.last_heartbeat.is_empty(),
            "heartbeat timestamp should be set"
        );
    }

    #[test]
    fn network_type_and_vxlan_fields_roundtrip() {
        let db = Database::open(":memory:").expect("open db");
        let node = test_node();
        db.upsert_node(&node).expect("insert node");

        db.insert_network(&NetworkRow {
            name: "overlay".to_string(),
            external_ip: "0.0.0.0".to_string(),
            gateway_ip: "10.200.0.1".to_string(),
            internal_netmask: "255.255.255.0".to_string(),
            node_id: node.id.clone(),
            allowed_tcp_ports: String::new(),
            allowed_udp_ports: String::new(),
            vlan_id: 0,
            network_type: "vxlan".to_string(),
            enable_outbound_nat: false,
            vni: 10042,
            next_ip: 2,
        })
        .expect("insert vxlan network");

        let got = db
            .get_network_for_node(&node.id, "overlay")
            .expect("get")
            .expect("exists");
        assert_eq!(got.network_type, "vxlan");
        assert!(!got.enable_outbound_nat);
        assert_eq!(got.vni, 10042);
        assert_eq!(got.next_ip, 2);
    }

    #[test]
    fn list_networks_by_name_returns_all_nodes() {
        let db = Database::open(":memory:").expect("open db");
        let mut n1 = test_node();
        n1.id = "n1".to_string();
        let mut n2 = test_node();
        n2.id = "n2".to_string();
        db.upsert_node(&n1).expect("insert n1");
        db.upsert_node(&n2).expect("insert n2");

        for nid in ["n1", "n2"] {
            db.insert_network(&NetworkRow {
                name: "shared".to_string(),
                external_ip: "0.0.0.0".to_string(),
                gateway_ip: "10.200.0.1".to_string(),
                internal_netmask: "255.255.255.0".to_string(),
                node_id: nid.to_string(),
                allowed_tcp_ports: String::new(),
                allowed_udp_ports: String::new(),
                vlan_id: 0,
                network_type: "vxlan".to_string(),
                enable_outbound_nat: true,
                vni: 10100,
                next_ip: 2,
            })
            .expect("insert network");
        }

        let all = db.list_networks_by_name("shared").expect("list");
        assert_eq!(all.len(), 2);
    }

    #[test]
    fn allocate_vm_ip_increments_sequentially() {
        let db = Database::open(":memory:").expect("open db");
        let node = test_node();
        db.upsert_node(&node).expect("insert node");

        db.insert_network(&NetworkRow {
            name: "vxnet".to_string(),
            external_ip: "0.0.0.0".to_string(),
            gateway_ip: "10.200.0.1".to_string(),
            internal_netmask: "255.255.255.0".to_string(),
            node_id: node.id.clone(),
            allowed_tcp_ports: String::new(),
            allowed_udp_ports: String::new(),
            vlan_id: 0,
            network_type: "vxlan".to_string(),
            enable_outbound_nat: true,
            vni: 10050,
            next_ip: 2,
        })
        .expect("insert network");

        let ip1 = db.allocate_vm_ip("vxnet", &node.id).expect("alloc 1");
        assert_eq!(ip1, "10.200.0.2");

        let ip2 = db.allocate_vm_ip("vxnet", &node.id).expect("alloc 2");
        assert_eq!(ip2, "10.200.0.3");

        let ip3 = db.allocate_vm_ip("vxnet", &node.id).expect("alloc 3");
        assert_eq!(ip3, "10.200.0.4");
    }

    #[test]
    fn allocate_vm_ip_global_is_unique_across_nodes() {
        let db = Database::open(":memory:").expect("open db");
        let mut n1 = test_node();
        n1.id = "n1".to_string();
        let mut n2 = test_node();
        n2.id = "n2".to_string();
        db.upsert_node(&n1).expect("insert n1");
        db.upsert_node(&n2).expect("insert n2");

        for nid in ["n1", "n2"] {
            db.insert_network(&NetworkRow {
                name: "overlay".to_string(),
                external_ip: "0.0.0.0".to_string(),
                gateway_ip: "10.50.0.1".to_string(),
                internal_netmask: "255.255.255.0".to_string(),
                node_id: nid.to_string(),
                allowed_tcp_ports: String::new(),
                allowed_udp_ports: String::new(),
                vlan_id: 0,
                network_type: "vxlan".to_string(),
                enable_outbound_nat: false,
                vni: 10200,
                next_ip: 2,
            })
            .expect("insert");
        }

        let ip1 = db.allocate_vm_ip_global("overlay").expect("alloc 1");
        assert_eq!(ip1, "10.50.0.2");

        let ip2 = db.allocate_vm_ip_global("overlay").expect("alloc 2");
        assert_eq!(ip2, "10.50.0.3");

        let ip3 = db.allocate_vm_ip_global("overlay").expect("alloc 3");
        assert_eq!(ip3, "10.50.0.4");

        let nets = db.list_networks_by_name("overlay").expect("list");
        for n in &nets {
            assert_eq!(n.next_ip, 5, "both node rows should be at 5");
        }
    }

    #[test]
    fn vxlan_released_ip_is_reused_and_skipped_while_still_assigned() {
        let db = Database::open(":memory:").expect("open db");
        let node = test_node();
        db.upsert_node(&node).expect("insert node");
        db.insert_network(&NetworkRow {
            name: "overlay".into(),
            external_ip: "0.0.0.0".into(),
            gateway_ip: "10.50.0.1".into(),
            internal_netmask: "255.255.255.0".into(),
            node_id: node.id.clone(),
            allowed_tcp_ports: String::new(),
            allowed_udp_ports: String::new(),
            vlan_id: 0,
            network_type: "vxlan".into(),
            enable_outbound_nat: false,
            vni: 10200,
            next_ip: 2,
        })
        .expect("insert network");

        let first = db.allocate_vm_ip_global("overlay").expect("alloc");
        assert_eq!(first, "10.50.0.2");
        let mut vm = test_vm(&node.id);
        vm.network = "overlay".into();
        vm.vm_ip = first.clone();
        db.insert_vm(&vm).expect("insert vm");
        assert!(db.delete_vm_by_id_or_name(&vm.id).expect("delete"));

        let mut still = test_vm(&node.id);
        still.id = "vm-hold".into();
        still.name = "hold".into();
        still.network = "overlay".into();
        still.vm_ip = first.clone();
        still.image_path = "/var/lib/kcore/images/hold.raw".into();
        db.insert_vm(&still).expect("hold the address");
        let skipped = db.allocate_vm_ip_global("overlay").expect("skip in-use");
        assert_eq!(skipped, "10.50.0.3");

        assert!(db.delete_vm_by_id_or_name(&still.id).expect("delete hold"));
        let reused = db.allocate_vm_ip_global("overlay").expect("reuse");
        assert_eq!(reused, "10.50.0.2");
        // .3 was handed out while .2 was still assigned, so the counter is already past it.
        let fresh = db.allocate_vm_ip_global("overlay").expect("fresh");
        assert_eq!(fresh, "10.50.0.4");
    }

    #[test]
    fn ipv6_allocates_from_the_prefix_and_reclaims_on_delete() {
        let db = Database::open(":memory:").expect("open db");
        let node = test_node();
        db.upsert_node(&node).expect("insert node");
        db.insert_network(&NetworkRow {
            name: "overlay".into(),
            external_ip: "0.0.0.0".into(),
            gateway_ip: "10.50.0.1".into(),
            internal_netmask: "255.255.255.0".into(),
            node_id: node.id.clone(),
            allowed_tcp_ports: String::new(),
            allowed_udp_ports: String::new(),
            vlan_id: 0,
            network_type: "vxlan".into(),
            enable_outbound_nat: true,
            vni: 1,
            next_ip: 2,
        })
        .expect("insert network");
        db.upsert_network_policy(&NetworkPolicyRow {
            node_id: node.id.clone(),
            name: "overlay".into(),
            east_west: true,
            ipv6_prefix: "fd00:10:240::/64".into(),
            ipv6_gateway: "fd00:10:240::1".into(),
            ipv6_next: 2,
        })
        .expect("policy");

        let mut vm = test_vm(&node.id);
        vm.network = "overlay".into();
        db.insert_vm(&vm).expect("insert vm");
        let first = db
            .allocate_vm_ipv6(&node.id, "overlay", &vm.id, 0, true)
            .expect("alloc v6");
        assert_eq!(first, "fd00:10:240::2");
        assert!(db.delete_vm_by_id_or_name(&vm.id).expect("delete"));

        let mut again = test_vm(&node.id);
        again.id = "vm-2".into();
        again.name = "web-2".into();
        again.network = "overlay".into();
        again.image_path = "/var/lib/kcore/images/web-2.raw".into();
        db.insert_vm(&again).expect("insert again");
        let reused = db
            .allocate_vm_ipv6(&node.id, "overlay", &again.id, 0, true)
            .expect("reuse v6");
        assert_eq!(reused, "fd00:10:240::2");
        let next = db
            .allocate_vm_ipv6(&node.id, "overlay", &again.id, 1, true)
            .expect("next v6");
        assert_eq!(next, "fd00:10:240::3");
    }

    #[test]
    fn vm_ip_stored_and_retrieved() {
        let db = Database::open(":memory:").expect("open db");
        let node = test_node();
        db.upsert_node(&node).expect("insert node");

        let mut vm = test_vm(&node.id);
        vm.vm_ip = "10.200.0.5".to_string();
        db.insert_vm(&vm).expect("insert vm");

        let got = db.get_vm("vm-1").expect("get").expect("exists");
        assert_eq!(got.vm_ip, "10.200.0.5");
    }

    #[test]
    fn workload_roundtrip_works() {
        let db = Database::open(":memory:").expect("open db");
        let node = test_node();
        db.upsert_node(&node).expect("insert node");

        let mut wl = test_workload(&node.id);
        db.upsert_workload(&wl).expect("upsert workload");

        let got = db
            .get_workload(&wl.id)
            .expect("get workload")
            .expect("exists");
        assert_eq!(got.kind, "container");
        assert_eq!(got.container_image, "nginx:alpine");

        let listed = db
            .list_workloads(Some("container"), Some(&node.id))
            .expect("list workloads");
        assert_eq!(listed.len(), 1);

        assert!(db
            .update_workload_runtime_state(&wl.id, "stopped")
            .expect("update state"));
        assert!(db
            .update_workload_desired_state(&wl.id, "stopped")
            .expect("update desired"));
        wl = db.get_workload(&wl.id).expect("reload").expect("exists");
        assert_eq!(wl.runtime_state, "stopped");
        assert_eq!(wl.desired_state, "stopped");

        assert!(db
            .delete_workload_by_id_or_name(&wl.id)
            .expect("delete workload"));
    }

    #[test]
    fn replication_outbox_append_and_count() {
        let db = Database::open(":memory:").expect("open db");
        assert_eq!(db.replication_outbox_len().expect("count"), 0);
        let id = db
            .append_replication_outbox("node.register", "node/n1", br#"{"x":1}"#)
            .expect("append");
        assert!(id >= 1);
        assert_eq!(db.replication_outbox_len().expect("count"), 1);
    }

    #[test]
    fn audit_events_append_list_and_filters() {
        let db = Database::open(":memory:").expect("open db");
        let id1 = db
            .append_audit_event("kctl", "CreateVm", "vm/web-01", "")
            .expect("append 1");
        let id2 = db
            .append_audit_event("kctl", "DeleteVm", "vm/web-01", r#"{"ok":true}"#)
            .expect("append 2");
        assert!(id2 > id1);

        let all = db.list_audit_events(0, None, None).expect("list");
        assert_eq!(all.len(), 2);
        assert_eq!(all[0].action, "DeleteVm");
        assert_eq!(all[1].action, "CreateVm");

        let filtered = db
            .list_audit_events(10, None, Some("CreateVm"))
            .expect("filter");
        assert_eq!(filtered.len(), 1);
        assert_eq!(filtered[0].resource, "vm/web-01");

        let limited = db.list_audit_events(1, None, None).expect("limit");
        assert_eq!(limited.len(), 1);
        assert_eq!(limited[0].id, id2);
    }

    #[test]
    fn audit_events_reject_update_and_delete() {
        let db = Database::open(":memory:").expect("open db");
        let id = db
            .append_audit_event("kctl", "ApproveNode", "node/n1", "")
            .expect("append");
        let conn = db.lock_conn().expect("lock");
        let upd = conn.execute(
            "UPDATE audit_events SET actor = 'x' WHERE id = ?1",
            params![id],
        );
        assert!(upd.is_err(), "update must be rejected");
        let del = conn.execute("DELETE FROM audit_events WHERE id = ?1", params![id]);
        assert!(del.is_err(), "delete must be rejected");
    }

    #[test]
    fn replication_outbox_list_since_orders_and_limits() {
        let db = Database::open(":memory:").expect("open db");
        let id1 = db
            .append_replication_outbox("node.register", "node/n1", br#"{"seq":1}"#)
            .expect("append 1");
        let _id2 = db
            .append_replication_outbox("vm.create", "vm/v1", br#"{"seq":2}"#)
            .expect("append 2");

        let rows = db
            .list_replication_outbox_since(id1, 10)
            .expect("list since id1");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].event_type, "vm.create");
        assert_eq!(rows[0].resource_key, "vm/v1");
        assert_eq!(rows[0].payload, br#"{"seq":2}"#);

        let all = db
            .list_replication_outbox_since(0, 1)
            .expect("list limited");
        assert_eq!(all.len(), 1);
        assert_eq!(all[0].id, id1);
    }

    #[test]
    fn replication_ack_upsert_and_read() {
        let db = Database::open(":memory:").expect("open db");
        assert_eq!(db.get_replication_ack("peer-a").expect("get"), None);

        db.upsert_replication_ack("peer-a", 42)
            .expect("upsert first");
        assert_eq!(db.get_replication_ack("peer-a").expect("get"), Some(42));

        db.upsert_replication_ack("peer-a", 105)
            .expect("upsert second");
        assert_eq!(db.get_replication_ack("peer-a").expect("get"), Some(105));

        db.upsert_replication_ack("peer-a", 7)
            .expect("upsert should remain monotonic");
        assert_eq!(db.get_replication_ack("peer-a").expect("get"), Some(105));
    }

    #[test]
    fn replication_outbox_head_id_empty_and_populated() {
        let db = Database::open(":memory:").expect("open db");
        assert_eq!(db.replication_outbox_head_id().expect("head"), 0);
        let id = db
            .append_replication_outbox("node.register", "node/n1", br#"{"x":1}"#)
            .expect("append");
        assert_eq!(db.replication_outbox_head_id().expect("head"), id);
    }

    #[test]
    fn replication_ack_list_returns_rows() {
        let db = Database::open(":memory:").expect("open db");
        db.upsert_replication_ack("peer-a", 7).expect("upsert a");
        db.upsert_replication_ack("pull/10.0.0.11:9090", 3)
            .expect("upsert pull");
        let rows = db.list_replication_acks().expect("list");
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].peer_id, "peer-a");
        assert_eq!(rows[0].last_event_id, 7);
        assert!(!rows[0].updated_at.is_empty());
    }

    #[test]
    fn replication_received_op_insert_and_exists() {
        let db = Database::open(":memory:").expect("open db");
        assert!(!db
            .replication_received_op_exists("op-1")
            .expect("exists before"));
        db.insert_replication_received_op("op-1", "ctrl-a", "vm.create", "vm/v1")
            .expect("insert");
        assert!(db
            .replication_received_op_exists("op-1")
            .expect("exists after"));
    }

    #[test]
    fn replication_resource_head_roundtrip() {
        let db = Database::open(":memory:").expect("open db");
        let row = ReplicationResourceHeadRow {
            resource_key: "vm/v1".into(),
            last_op_id: "op-1".into(),
            last_logical_ts_unix_ms: 123,
            last_policy_priority: 10,
            last_intent_epoch: 3,
            last_validity: "valid".into(),
            last_safety_class: "safe".into(),
            last_controller_id: "ctrl-a".into(),
            last_event_id: 7,
            last_event_type: "vm.update".into(),
            last_body_json: "{\"cpu\":2}".into(),
        };
        db.upsert_replication_resource_head(&row).expect("upsert");
        let got = db
            .get_replication_resource_head("vm/v1")
            .expect("get")
            .expect("exists");
        assert_eq!(got.last_op_id, "op-1");
        assert_eq!(got.last_logical_ts_unix_ms, 123);
        assert_eq!(got.last_policy_priority, 10);
        assert_eq!(got.last_intent_epoch, 3);
        assert_eq!(got.last_event_type, "vm.update");
    }

    #[test]
    fn replication_conflict_insert_and_query() {
        let db = Database::open(":memory:").expect("open db");
        let id = db
            .insert_replication_conflict(
                "vm/v1",
                "op-old",
                "op-new",
                "ctrl-a",
                "ctrl-b",
                "same logical timestamp",
            )
            .expect("insert conflict");
        assert!(id >= 1);
        assert_eq!(
            db.count_unresolved_replication_conflicts()
                .expect("count conflicts"),
            1
        );
        let rows = db
            .list_unresolved_replication_conflicts(10)
            .expect("list conflicts");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].resource_key, "vm/v1");
        assert_eq!(rows[0].challenger_controller_id, "ctrl-b");
        assert!(db
            .resolve_replication_conflict(rows[0].id)
            .expect("resolve conflict"));
        assert_eq!(
            db.count_unresolved_replication_conflicts()
                .expect("count after resolve"),
            0
        );
    }

    #[test]
    fn compensation_job_roundtrip() {
        let db = Database::open(":memory:").expect("open db");
        let conflict_id = db
            .insert_replication_conflict(
                "vm/v1",
                "op-inc",
                "op-loser",
                "ctrl-a",
                "ctrl-b",
                "needs compensation",
            )
            .expect("insert conflict");
        assert_eq!(db.count_pending_compensation_jobs().expect("count"), 0);
        let job_id = db
            .insert_compensation_job(
                conflict_id,
                "vm/v1",
                "op-loser",
                "vm.update",
                r#"{"vmId":"vm/v1","cpu":4}"#,
            )
            .expect("insert job");
        assert!(job_id >= 1);
        assert_eq!(db.count_pending_compensation_jobs().expect("count"), 1);
        let job = db
            .claim_next_compensation_job()
            .expect("claim")
            .expect("job exists");
        assert_eq!(job.status, "running");
        assert_eq!(job.conflict_id, conflict_id);
        assert_eq!(
            db.get_compensation_job_status_for_loser_op("op-loser")
                .expect("status lookup"),
            Some("running".to_string())
        );
        db.complete_compensation_job(job.id).expect("complete");
        assert_eq!(db.count_pending_compensation_jobs().expect("count"), 0);
        assert_eq!(
            db.get_compensation_job_status_for_loser_op("op-loser")
                .expect("status lookup"),
            Some("completed".to_string())
        );
    }

    #[test]
    fn materialized_head_roundtrip() {
        let db = Database::open(":memory:").expect("open db");
        assert!(db
            .get_materialized_replication_head("vm/v1")
            .expect("get")
            .is_none());
        db.upsert_materialized_replication_head("vm/v1", "op-1", "vm.update")
            .expect("upsert");
        let row = db
            .get_materialized_replication_head("vm/v1")
            .expect("get")
            .expect("present");
        assert_eq!(row.resource_key, "vm/v1");
        assert_eq!(row.last_op_id, "op-1");
        assert_eq!(row.last_event_type, "vm.update");
    }

    #[test]
    fn replication_reservation_roundtrip() {
        let db = Database::open(":memory:").expect("open db");
        db.upsert_replication_reservation("node-capacity/node-1", "vm/v1", "op-1", "reserved", "")
            .expect("upsert reservation");
        let row = db
            .get_replication_reservation("node-capacity/node-1", "vm/v1")
            .expect("get reservation")
            .expect("reservation present");
        assert_eq!(row.op_id, "op-1");
        assert_eq!(row.status, "reserved");
        assert!(row.error.is_empty());
        assert_eq!(row.retry_count, 0);
    }

    #[test]
    fn replication_reservation_failure_classification_and_budget() {
        let db = Database::open(":memory:").expect("open db");
        let (status1, retry1) = db
            .record_replication_reservation_failure(
                "node-capacity/node-x",
                "vm/v1",
                "op-1",
                true,
                "node not ready",
                3,
            )
            .expect("failure 1");
        assert_eq!(status1, "failed_retryable");
        assert_eq!(retry1, 1);
        let (status2, retry2) = db
            .record_replication_reservation_failure(
                "node-capacity/node-x",
                "vm/v1",
                "op-1",
                true,
                "node not ready",
                3,
            )
            .expect("failure 2");
        assert_eq!(status2, "failed_retryable");
        assert_eq!(retry2, 2);
        let (status3, retry3) = db
            .record_replication_reservation_failure(
                "node-capacity/node-x",
                "vm/v1",
                "op-1",
                true,
                "node not ready",
                3,
            )
            .expect("failure 3");
        assert_eq!(status3, "retry_exhausted");
        assert_eq!(retry3, 3);
        assert_eq!(
            db.count_failed_retryable_replication_reservations()
                .expect("count retryable"),
            0
        );
        assert_eq!(
            db.count_failed_replication_reservations()
                .expect("count failed total"),
            1
        );
        assert_eq!(
            db.count_retry_exhausted_replication_reservations()
                .expect("count exhausted"),
            1
        );
    }

    #[test]
    fn list_retryable_replication_reservations_returns_only_retryable() {
        let db = Database::open(":memory:").expect("open db");
        db.upsert_replication_reservation_with_retry(
            "node-capacity/node-a",
            "vm/v-a",
            "op-a",
            "failed_retryable",
            "node not ready",
            1,
        )
        .expect("insert retryable");
        db.upsert_replication_reservation_with_retry(
            "node-capacity/node-b",
            "vm/v-b",
            "op-b",
            "failed_non_retryable",
            "node missing",
            1,
        )
        .expect("insert non-retryable");
        let rows = db
            .list_retryable_replication_reservations(10, 0)
            .expect("list retryable");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].resource_key, "vm/v-a");
    }

    #[test]
    fn replication_metrics_queries_work() {
        let db = Database::open(":memory:").expect("open db");
        db.insert_replication_conflict("vm/v1", "op-a", "op-b", "ctrl-a", "ctrl-b", "conflict")
            .expect("insert conflict");
        db.upsert_replication_reservation(
            "node-capacity/node-1",
            "vm/v1",
            "op-1",
            "failed_non_retryable",
            "x",
        )
        .expect("insert failed reservation");
        db.upsert_replication_resource_head(&ReplicationResourceHeadRow {
            resource_key: "vm/v1".to_string(),
            last_op_id: "op-1".to_string(),
            last_logical_ts_unix_ms: 1,
            last_policy_priority: 0,
            last_intent_epoch: 0,
            last_validity: "valid".to_string(),
            last_safety_class: "safe".to_string(),
            last_controller_id: "ctrl-a".to_string(),
            last_event_id: 1,
            last_event_type: "vm.update".to_string(),
            last_body_json: "{}".to_string(),
        })
        .expect("insert head");
        db.upsert_materialized_replication_head("vm/v1", "op-0", "vm.update")
            .expect("insert stale materialized head");

        assert_eq!(
            db.count_failed_replication_reservations()
                .expect("count failed reservations"),
            1
        );
        assert_eq!(
            db.count_replication_materialization_backlog()
                .expect("materialization backlog"),
            1
        );
        assert!(
            db.oldest_unresolved_conflict_age_seconds()
                .expect("oldest unresolved age")
                >= 0
        );
    }

    #[test]
    fn security_group_roundtrip_with_rules() {
        let db = Database::open(":memory:").expect("db");
        db.upsert_security_group(&SecurityGroupRow {
            name: "web".to_string(),
            description: "web ingress".to_string(),
            created_at: String::new(),
        })
        .expect("insert sg");
        db.replace_security_group_rules(
            "web",
            &[SecurityGroupRuleRow {
                id: "r1".to_string(),
                security_group: "web".to_string(),
                protocol: "tcp".to_string(),
                host_port: 443,
                target_port: 8443,
                source_cidr: "0.0.0.0/0".to_string(),
                target_vm: "vm-1".to_string(),
                enable_dnat: true,
            }],
        )
        .expect("insert rule");

        let got = db.get_security_group("web").expect("read").expect("exists");
        assert_eq!(got.name, "web");
        assert_eq!(got.description, "web ingress");
        let rules = db.list_security_group_rules("web").expect("rules");
        assert_eq!(rules.len(), 1);
        assert_eq!(rules[0].protocol, "tcp");
        assert_eq!(rules[0].host_port, 443);
        assert!(rules[0].enable_dnat);
    }

    /// Regression: node reassignment used to delete and re-insert the `vms`
    /// row. Both child tables keyed on `vms(id)` cascade on delete, so every
    /// migrate and drain silently dropped the VM's security groups.
    #[test]
    fn set_vm_node_moves_a_vm_without_dropping_its_child_rows() {
        let db = Database::open(":memory:").expect("open db");
        let mut node_a = test_node();
        node_a.id = "node-a".to_string();
        db.upsert_node(&node_a).expect("node-a");
        let mut node_b = test_node();
        node_b.id = "node-b".to_string();
        node_b.address = "10.0.0.2:9091".to_string();
        db.upsert_node(&node_b).expect("node-b");

        let vm = test_vm("node-a");
        db.insert_vm(&vm).expect("insert vm");
        db.insert_ssh_key("ops", "ssh-ed25519 AAAAtest ops@kcore")
            .expect("ssh key");
        db.associate_vm_ssh_keys(&vm.id, &["ops".to_string()])
            .expect("associate key");
        db.upsert_security_group(&SecurityGroupRow {
            name: "web".to_string(),
            description: String::new(),
            created_at: String::new(),
        })
        .expect("security group");
        db.attach_security_group_to_vm("web", &vm.id)
            .expect("attach");

        assert!(db.set_vm_node(&vm.id, "node-b").expect("reassign"));

        let moved = db.get_vm(&vm.id).expect("get vm").expect("vm exists");
        assert_eq!(moved.node_id, "node-b");
        assert_eq!(
            db.list_security_groups_for_vm(&vm.id).expect("groups"),
            vec!["web".to_string()]
        );
        assert_eq!(
            db.get_vm_ssh_key_names(&vm.id).expect("keys"),
            vec!["ops".to_string()]
        );
        assert!(db.list_vms_for_node("node-a").expect("node-a").is_empty());
        assert_eq!(db.list_vms_for_node("node-b").expect("node-b").len(), 1);
    }

    #[test]
    fn apply_vm_runtime_state_reports_missing_unchanged_and_changed() {
        let db = Database::open(":memory:").expect("open db");
        let node = test_node();
        db.upsert_node(&node).expect("node");
        let vm = test_vm(&node.id);
        db.insert_vm(&vm).expect("insert vm");

        assert_eq!(
            db.apply_vm_runtime_state(&node.id, "missing", "running")
                .expect("missing"),
            VmRuntimeApply::Missing
        );
        assert_eq!(
            db.apply_vm_runtime_state(&node.id, &vm.name, "unknown")
                .expect("same state"),
            VmRuntimeApply::Unchanged
        );
        match db
            .apply_vm_runtime_state(&node.id, &vm.name, "running")
            .expect("change")
        {
            VmRuntimeApply::Changed(change) => {
                assert_eq!(change.vm_id, vm.id);
                assert_eq!(change.previous, "unknown");
                assert_eq!(change.current, "running");
            }
            other => panic!("expected a change, got {other:?}"),
        }
        assert_eq!(
            db.get_vm(&vm.id).expect("get").expect("vm").runtime_state,
            "running"
        );
        assert_eq!(
            db.apply_vm_runtime_state(&node.id, &vm.name, "running")
                .expect("second write"),
            VmRuntimeApply::Unchanged
        );
    }

    fn set_vm_node_reports_a_missing_vm_rather_than_creating_one() {
        let db = Database::open(":memory:").expect("open db");
        let node = test_node();
        db.upsert_node(&node).expect("node");
        assert!(!db.set_vm_node("vm-nope", &node.id).expect("update"));
    }

    #[test]
    fn extra_nics_round_trip_and_cascade_with_the_vm() {
        let db = Database::open(":memory:").expect("open db");
        let node = test_node();
        db.upsert_node(&node).expect("node");
        let vm = test_vm(&node.id);
        db.insert_vm(&vm).expect("insert vm");
        db.insert_vm_nic(&VmNicRow {
            vm_id: vm.id.clone(),
            position: 1,
            network: "backend".into(),
            mac_address: "52:54:00:11:22:33".into(),
            model: "virtio".into(),
            ip_address: "10.1.0.8".into(),
        })
        .expect("insert nic");
        let listed = db.list_vm_nics(&vm.id).expect("list");
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].network, "backend");
        assert_eq!(
            db.list_nic_ips_on_node_network(&node.id, "backend")
                .expect("ips"),
            vec!["10.1.0.8".to_string()]
        );
        db.delete_vm_by_id_or_name(&vm.id).expect("delete");
        assert!(db.list_vm_nics(&vm.id).expect("after delete").is_empty());
    }

    #[test]
    fn set_vm_node_rejects_an_unknown_target_node() {
        let db = Database::open(":memory:").expect("open db");
        let node = test_node();
        db.upsert_node(&node).expect("node");
        let vm = test_vm(&node.id);
        db.insert_vm(&vm).expect("insert vm");
        db.set_vm_node(&vm.id, "node-does-not-exist")
            .expect_err("the node_id foreign key must reject an unknown node");
        assert_eq!(
            db.get_vm(&vm.id).expect("get vm").expect("vm").node_id,
            node.id
        );
    }

    /// Replication replays `vm.create` for VMs a peer already holds; emulating
    /// that with delete-then-insert would cascade the child rows away.
    #[test]
    fn upsert_vm_updates_in_place_and_keeps_child_rows() {
        let db = Database::open(":memory:").expect("open db");
        let node = test_node();
        db.upsert_node(&node).expect("node");
        let vm = test_vm(&node.id);
        db.insert_vm(&vm).expect("insert vm");
        db.upsert_security_group(&SecurityGroupRow {
            name: "web".to_string(),
            description: String::new(),
            created_at: String::new(),
        })
        .expect("security group");
        db.attach_security_group_to_vm("web", &vm.id)
            .expect("attach");

        let mut updated = vm.clone();
        updated.cpu = 8;
        updated.vm_ip = "10.240.0.9".to_string();
        db.upsert_vm(&updated).expect("upsert existing vm");

        let stored = db.get_vm(&vm.id).expect("get vm").expect("vm exists");
        assert_eq!(stored.cpu, 8);
        assert_eq!(stored.vm_ip, "10.240.0.9");
        assert_eq!(
            db.list_security_groups_for_vm(&vm.id).expect("groups"),
            vec!["web".to_string()],
            "an upsert must not cascade the VM's attachments away"
        );
        assert_eq!(db.list_vms().expect("list").len(), 1);
    }

    #[test]
    fn security_group_attachments_roundtrip() {
        let db = Database::open(":memory:").expect("db");
        let node = NodeRow {
            id: "node-1".to_string(),
            hostname: "node-1".to_string(),
            address: "10.0.0.1:9091".to_string(),
            cpu_cores: 4,
            memory_bytes: 8 * 1024 * 1024 * 1024,
            status: "ready".to_string(),
            last_heartbeat: String::new(),
            gateway_interface: "eno1".to_string(),
            cpu_used: 0,
            memory_used: 0,
            storage_backend: "filesystem".to_string(),
            disable_vxlan: false,
            approval_status: "approved".to_string(),
            cert_expiry_days: -1,
            luks_method: String::new(),
            dc_id: "DC1".to_string(),
        };
        db.upsert_node(&node).expect("node");
        db.insert_vm(&VmRow {
            id: "vm-1".to_string(),
            name: "vm-1".to_string(),
            cpu: 2,
            memory_bytes: 1024,
            image_path: "/tmp/img.raw".to_string(),
            image_url: String::new(),
            image_sha256: String::new(),
            image_format: "raw".to_string(),
            image_size: 1024,
            network: "private".to_string(),
            auto_start: true,
            node_id: "node-1".to_string(),
            created_at: String::new(),
            runtime_state: "running".to_string(),
            cloud_init_user_data: String::new(),
            storage_backend: "filesystem".to_string(),
            storage_size_bytes: 1024 * 1024,
            vm_ip: "10.240.0.22".to_string(),
            pci_devices: String::new(),
        })
        .expect("vm");
        db.insert_network(&NetworkRow {
            name: "private".to_string(),
            external_ip: "203.0.113.10".to_string(),
            gateway_ip: "10.240.0.1".to_string(),
            internal_netmask: "255.255.255.0".to_string(),
            node_id: "node-1".to_string(),
            allowed_tcp_ports: String::new(),
            allowed_udp_ports: String::new(),
            vlan_id: 0,
            network_type: "nat".to_string(),
            enable_outbound_nat: true,
            vni: 0,
            next_ip: 2,
        })
        .expect("network");
        db.upsert_security_group(&SecurityGroupRow {
            name: "web".to_string(),
            description: String::new(),
            created_at: String::new(),
        })
        .expect("sg");
        db.attach_security_group_to_vm("web", "vm-1")
            .expect("attach vm");
        db.attach_security_group_to_network("web", "private", "node-1")
            .expect("attach network");

        let vm_groups = db.list_security_groups_for_vm("vm-1").expect("vm groups");
        let net_groups = db
            .list_security_groups_for_network("private", "node-1")
            .expect("net groups");
        assert_eq!(vm_groups, vec!["web".to_string()]);
        assert_eq!(net_groups, vec!["web".to_string()]);
    }

    #[test]
    fn upsert_peer_address_only_preserves_dc_id() {
        let db = Database::open(":memory:").expect("open db");
        db.upsert_controller_peer("ctrl-dc2", "10.0.1.50:9090", "DC2")
            .expect("full upsert");
        let peers = db.list_controller_peers().expect("list");
        assert_eq!(peers.len(), 1);
        assert_eq!(peers[0].dc_id, "DC2");

        db.upsert_controller_peer_address_only("ctrl-dc2", "10.0.1.50:9091")
            .expect("address-only upsert");
        let peers = db.list_controller_peers().expect("list");
        assert_eq!(peers.len(), 1);
        assert_eq!(peers[0].dc_id, "DC2");
        assert_eq!(peers[0].address, "10.0.1.50:9091");
    }

    #[test]
    fn upsert_peer_address_only_inserts_with_default_dc() {
        let db = Database::open(":memory:").expect("open db");
        db.upsert_controller_peer_address_only("ctrl-new", "10.0.2.1:9090")
            .expect("address-only insert");
        let peers = db.list_controller_peers().expect("list");
        assert_eq!(peers.len(), 1);
        assert_eq!(peers[0].dc_id, "DC1");
        assert_eq!(peers[0].address, "10.0.2.1:9090");
    }

    #[test]
    fn ceph_cluster_crud_and_status_cascade() {
        let db = Database::open(":memory:").expect("open db");
        let row = CephClusterRow {
            name: "lab".into(),
            generation: 1,
            spec_json: r#"{"fsid":"f","publicNetwork":"10.0.0.0/24","clusterNetwork":"10.1.0.0/24","size":3,"minSize":2,"nodes":[]}"#.into(),
            bootstrap_json: String::new(),
            created_at: String::new(),
            updated_at: String::new(),
        };
        db.upsert_ceph_cluster(&row).expect("upsert cluster");
        let got = db.get_ceph_cluster("lab").expect("get").expect("exists");
        assert_eq!(got.generation, 1);
        assert_eq!(db.list_ceph_clusters().unwrap().len(), 1);

        db.upsert_ceph_cluster_status(&CephClusterStatusRow {
            name: "lab".into(),
            observed_generation: 0,
            phase: "pending".into(),
            health_message: String::new(),
            ceph_status_json: String::new(),
            last_transition_at: String::new(),
        })
        .expect("status");
        assert!(db.get_ceph_cluster_status("lab").unwrap().is_some());

        assert!(db.delete_ceph_cluster("lab").unwrap());
        assert!(db.get_ceph_cluster("lab").unwrap().is_none());
        assert!(
            db.get_ceph_cluster_status("lab").unwrap().is_none(),
            "status must cascade-delete with cluster"
        );
        assert!(!db.delete_ceph_cluster("lab").unwrap());
    }

    #[test]
    fn ceph_cluster_reconcile_queue_tracks_observed_generation() {
        let db = Database::open(":memory:").expect("open db");
        db.upsert_ceph_cluster(&CephClusterRow {
            name: "lab".into(),
            generation: 2,
            spec_json: "{}".into(),
            bootstrap_json: String::new(),
            created_at: String::new(),
            updated_at: String::new(),
        })
        .unwrap();
        assert!(
            db.list_ceph_clusters_needing_reconcile()
                .unwrap()
                .iter()
                .any(|c| c.name == "lab"),
            "fresh cluster must be queued"
        );

        db.upsert_ceph_cluster_status(&CephClusterStatusRow {
            name: "lab".into(),
            observed_generation: 2,
            phase: "healthy".into(),
            health_message: "HEALTH_OK".into(),
            ceph_status_json: String::new(),
            last_transition_at: String::new(),
        })
        .unwrap();
        assert!(
            !db.list_ceph_clusters_needing_reconcile()
                .unwrap()
                .iter()
                .any(|c| c.name == "lab"),
            "caught-up status must leave queue"
        );

        db.upsert_ceph_cluster(&CephClusterRow {
            name: "lab".into(),
            generation: 3,
            spec_json: "{\"bumped\":true}".into(),
            bootstrap_json: String::new(),
            created_at: String::new(),
            updated_at: String::new(),
        })
        .unwrap();
        assert!(
            db.list_ceph_clusters_needing_reconcile()
                .unwrap()
                .iter()
                .any(|c| c.name == "lab"),
            "generation bump must re-queue"
        );
    }

    fn test_root_volume(id: &str, vm_id: &str, vm_name: &str, size_bytes: i64) -> VolumeRow {
        VolumeRow {
            id: id.into(),
            name: format!("{vm_name}-root"),
            vm_id: vm_id.into(),
            role: VolumeRow::ROLE_ROOT.into(),
            slot: 0,
            pool: "kcore-vms".into(),
            image: format!("kcore-{vm_id}"),
            size_bytes,
            storage_class: "ceph".into(),
            attach_state: VolumeRow::ATTACH_ATTACHED.into(),
            serial: VolumeRow::serial_from_id(id),
            source_json: "{}".into(),
            guest_format_json: String::new(),
            generation: 1,
            parent_snapshot_id: String::new(),
            guest_visible_bytes: -1,
            guest_checked_at: String::new(),
            encrypted: false,
            wrapped_dek: String::new(),
            created_at: String::new(),
        }
    }

    #[test]
    fn volume_upsert_get_list_delete_by_vm() {
        let db = Database::open(":memory:").expect("open db");
        db.upsert_node(&test_node()).unwrap();
        db.insert_vm(&test_vm("n1")).unwrap();

        db.upsert_volume(&test_root_volume(
            "vol-1",
            "vm-1",
            "vm-1",
            10 * 1024 * 1024 * 1024,
        ))
        .unwrap();
        let got = db.get_volume_by_vm("vm-1").unwrap().expect("volume");
        assert_eq!(got.pool, "kcore-vms");
        assert_eq!(got.image, "kcore-vm-1");
        assert_eq!(got.role, "root");
        assert_eq!(db.list_volumes().unwrap().len(), 1);

        let mut resized = test_root_volume("vol-1", "vm-1", "vm-1", 20 * 1024 * 1024 * 1024);
        resized.image = "kcore-vm-1-resized".into();
        db.upsert_volume(&resized).unwrap();
        let updated = db.get_volume_by_vm("vm-1").unwrap().unwrap();
        assert_eq!(updated.image, "kcore-vm-1-resized");
        assert_eq!(updated.size_bytes, 20 * 1024 * 1024 * 1024);
        assert_eq!(db.list_volumes().unwrap().len(), 1, "upsert by id");

        let mut data = VolumeRow::new_data("pgdata", 5 * 1024 * 1024 * 1024);
        data.vm_id = "vm-1".into();
        data.attach_state = VolumeRow::ATTACH_ATTACHED.into();
        data.slot = 1;
        db.upsert_volume(&data).unwrap();
        assert_eq!(db.list_volumes_for_vm("vm-1").unwrap().len(), 2);
        assert_eq!(db.list_attached_volumes_for_vm("vm-1").unwrap().len(), 2);

        assert!(db.delete_volume_by_vm("vm-1").unwrap());
        assert!(db.get_volume_by_vm("vm-1").unwrap().is_none());
        assert!(!db.delete_volume_by_vm("vm-1").unwrap());
    }

    #[test]
    fn vm_operation_insert_open_unique_and_cancel() {
        let db = Database::open(":memory:").expect("open db");
        db.upsert_node(&test_node()).unwrap();
        db.insert_vm(&test_vm("n1")).unwrap();

        let op = VmOperationRow {
            id: "op-1".into(),
            vm_id: "vm-1".into(),
            kind: VmOperationRow::KIND_LIVE_MIGRATE.into(),
            phase: VmOperationRow::PHASE_PREPARING.into(),
            source_node: "n1".into(),
            target_node: "n2".into(),
            cancel_requested: false,
            send_succeeded: false,
            detail_json: "{}".into(),
            started_at: String::new(),
            updated_at: String::new(),
            finished_at: String::new(),
        };
        db.insert_vm_operation(&op).unwrap();
        assert!(db.get_open_vm_operation("vm-1").unwrap().is_some());
        assert!(
            db.insert_vm_operation(&VmOperationRow {
                id: "op-2".into(),
                ..op.clone()
            })
            .is_err(),
            "only one open operation per VM"
        );
        assert!(db.request_cancel_vm_operation("op-1").unwrap());
        let got = db.get_vm_operation("op-1").unwrap().unwrap();
        assert!(got.cancel_requested);
        db.update_vm_operation_phase(
            "op-1",
            VmOperationRow::PHASE_CANCELLED,
            Some(false),
            None,
            true,
        )
        .unwrap();
        assert!(db.get_open_vm_operation("vm-1").unwrap().is_none());
        // Now a new open op is allowed.
        db.insert_vm_operation(&VmOperationRow {
            id: "op-3".into(),
            finished_at: String::new(),
            ..op
        })
        .unwrap();
        assert_eq!(db.list_vm_operations(true).unwrap().len(), 2);
    }

    #[test]
    fn snapshot_round_trip_restores_nodes_and_rejects_garbage() {
        let src = Database::open(":memory:").expect("open");
        let node = test_node();
        src.upsert_node(&node).expect("insert");
        src.mark_node_config_push(&node.id).expect("flag");
        let (bytes, version) = src.snapshot_sqlite().expect("snapshot");
        assert_eq!(version, SCHEMA_VERSION);
        assert!(bytes.starts_with(b"SQLite format 3\0"));

        let dst = Database::open(":memory:").expect("open dest");
        let restored = dst.restore_sqlite(&bytes).expect("restore");
        assert_eq!(restored, SCHEMA_VERSION);
        let got = dst.get_node(&node.id).expect("get").expect("node");
        assert_eq!(got.hostname, node.hostname);
        assert!(dst.node_needs_config_push(&node.id).expect("flag"));

        let err = dst.restore_sqlite(b"not a database").expect_err("garbage");
        assert!(err.contains("not a SQLite"));
    }
}

/// Property-based tests (Phase 3) — database CRUD invariants.
///
/// `db.rs` is the source of truth for the entire system: a subtle bug
/// here (a VM that survives its node, an upsert that creates a duplicate,
/// a heartbeat that flips approval state) propagates silently and can
/// cause wrong Nix configs to be pushed. The example tests in
/// `mod tests` cover one concrete row at a time; these proptests cover
/// the same invariants over thousands of randomised rows.
///
/// Each test opens a fresh `:memory:` SQLite database so cases are
/// independent. The `Database::open(":memory:")` call is cheap (~µs)
/// because the in-memory file lives only for the duration of the test.
#[cfg(test)]
mod proptests {
    use super::*;
    use proptest::prelude::*;

    /// Build a `NodeRow` with caller-supplied identifying / quantitative
    /// fields and sensible defaults for the rest. Centralised so each
    /// test only has to randomise the fields it cares about.
    fn make_node(
        id: &str,
        hostname: &str,
        address: &str,
        cpu_cores: i32,
        memory_bytes: i64,
        cpu_used: i32,
        memory_used: i64,
        disable_vxlan: bool,
        cert_expiry_days: i32,
        dc_id: &str,
    ) -> NodeRow {
        NodeRow {
            id: id.to_string(),
            hostname: hostname.to_string(),
            address: address.to_string(),
            cpu_cores,
            memory_bytes,
            status: "ready".to_string(),
            last_heartbeat: String::new(),
            gateway_interface: "eno1".to_string(),
            cpu_used,
            memory_used,
            storage_backend: "filesystem".to_string(),
            disable_vxlan,
            approval_status: "approved".to_string(),
            cert_expiry_days,
            luks_method: String::new(),
            dc_id: dc_id.to_string(),
        }
    }

    /// Build a minimal `VmRow` for a given node, randomising only the
    /// fields each test cares about.
    #[allow(clippy::too_many_arguments)]
    fn make_vm(
        id: &str,
        name: &str,
        node_id: &str,
        cpu: i32,
        memory_bytes: i64,
        auto_start: bool,
    ) -> VmRow {
        VmRow {
            id: id.to_string(),
            name: name.to_string(),
            cpu,
            memory_bytes,
            image_path: format!("/var/lib/kcore/images/{name}.raw"),
            image_url: String::new(),
            image_sha256: String::new(),
            image_format: "raw".to_string(),
            image_size: 8192,
            network: "default".to_string(),
            auto_start,
            node_id: node_id.to_string(),
            created_at: String::new(),
            runtime_state: "unknown".to_string(),
            cloud_init_user_data: String::new(),
            storage_backend: "filesystem".to_string(),
            storage_size_bytes: 0,
            vm_ip: String::new(),
            pci_devices: String::new(),
        }
    }

    proptest! {
        #![proptest_config(ProptestConfig {
            // SQLite open + a handful of statements per case is fast
            // enough for ~256 cases per test; 2 000 (the controller-
            // pure proptest budget) would add noticeable wall time
            // without finding new bugs.
            cases: 256,
            .. ProptestConfig::default()
        })]

        /// **Node CRUD round-trip**: for any randomised `NodeRow`,
        /// `upsert_node` followed by `get_node` returns a row whose
        /// scalar fields equal the inserted ones.
        #[test]
        fn node_upsert_then_get_returns_same_row(
            id in "[a-z0-9-]{1,12}",
            hostname in "[a-z0-9-]{1,12}",
            address in "[a-z0-9.:-]{1,24}",
            cpu_cores in 1i32..=128,
            memory_bytes in 1i64..(1i64 << 40),
            cpu_used in 0i32..=128,
            memory_used in 0i64..(1i64 << 40),
            disable_vxlan in any::<bool>(),
            cert_expiry_days in -1i32..=3650,
            dc_id in prop::sample::select(vec!["DC1", "DC2", "EU-W"]),
        ) {
            let db = Database::open(":memory:").expect("open db");
            let node = make_node(
                &id, &hostname, &address,
                cpu_cores, memory_bytes,
                cpu_used, memory_used,
                disable_vxlan, cert_expiry_days,
                dc_id,
            );
            db.upsert_node(&node).unwrap();

            let got = db.get_node(&id).unwrap().expect("node exists after upsert");
            prop_assert_eq!(&got.id, &node.id);
            prop_assert_eq!(&got.hostname, &node.hostname);
            prop_assert_eq!(&got.address, &node.address);
            prop_assert_eq!(got.cpu_cores, node.cpu_cores);
            prop_assert_eq!(got.memory_bytes, node.memory_bytes);
            prop_assert_eq!(got.cpu_used, node.cpu_used);
            prop_assert_eq!(got.memory_used, node.memory_used);
            prop_assert_eq!(got.disable_vxlan, node.disable_vxlan);
            prop_assert_eq!(got.cert_expiry_days, node.cert_expiry_days);
            prop_assert_eq!(&got.dc_id, &node.dc_id);
        }

        /// **Upsert idempotence**: upserting the same node twice yields
        /// a single row in `list_nodes` (no PK duplicate, no orphan).
        #[test]
        fn node_upsert_is_idempotent(
            id in "[a-z0-9-]{1,12}",
            cpu_cores in 1i32..=128,
            memory_bytes in 1i64..(1i64 << 40),
        ) {
            let db = Database::open(":memory:").expect("open db");
            let node = make_node(
                &id, "h", "127.0.0.1:9091",
                cpu_cores, memory_bytes,
                0, 0, false, 0, "DC1",
            );
            db.upsert_node(&node).unwrap();
            db.upsert_node(&node).unwrap();

            let nodes = db.list_nodes().unwrap();
            prop_assert_eq!(nodes.len(), 1);
            prop_assert_eq!(&nodes[0].id, &id);
            prop_assert_eq!(nodes[0].cpu_cores, cpu_cores);
            prop_assert_eq!(nodes[0].memory_bytes, memory_bytes);
        }

        /// **Upsert updates fields in place**: upserting a node twice
        /// with different `address` values yields exactly one row
        /// whose address is the most recently provided one.
        #[test]
        fn node_upsert_updates_address_in_place(
            id in "[a-z0-9-]{1,12}",
            addr_a in "[a-z0-9.:-]{1,24}",
            addr_b in "[a-z0-9.:-]{1,24}",
        ) {
            prop_assume!(addr_a != addr_b);
            let db = Database::open(":memory:").expect("open db");
            let mut node = make_node(
                &id, "h", &addr_a, 1, 1024, 0, 0, false, 0, "DC1",
            );
            db.upsert_node(&node).unwrap();
            node.address = addr_b.clone();
            db.upsert_node(&node).unwrap();

            let nodes = db.list_nodes().unwrap();
            prop_assert_eq!(nodes.len(), 1);
            prop_assert_eq!(&nodes[0].address, &addr_b);
        }

        /// **Foreign-key integrity on VMs**: inserting a `VmRow` whose
        /// `node_id` does not exist in `nodes` MUST fail. SQLite's
        /// `PRAGMA foreign_keys=ON` is enabled in `Database::open`, so
        /// this is the property that protects the controller from
        /// scheduling a workload onto a phantom node.
        #[test]
        fn vm_insert_rejects_unknown_node_id(
            present_node in "[a-z0-9-]{1,8}",
            missing_node in "[a-z0-9-]{1,8}",
            vm_id in "[a-z0-9-]{1,8}",
        ) {
            prop_assume!(present_node != missing_node);
            let db = Database::open(":memory:").expect("open db");
            db.upsert_node(&make_node(
                &present_node, "h", "127.0.0.1:9091",
                1, 1024, 0, 0, false, 0, "DC1",
            )).unwrap();

            let vm = make_vm(&vm_id, &vm_id, &missing_node, 1, 1024, false);
            let err = db.insert_vm(&vm).unwrap_err();
            // Structured match: rusqlite surfaces FK violations as
            // SqliteFailure with code = ConstraintViolation and
            // extended_code = SQLITE_CONSTRAINT_FOREIGNKEY (787).
            // The integer literal is used because rusqlite does not
            // re-export the extended-code constant publicly.
            const SQLITE_CONSTRAINT_FOREIGNKEY: i32 = 787;
            match &err {
                rusqlite::Error::SqliteFailure(sqlite_err, _)
                    if sqlite_err.code == rusqlite::ErrorCode::ConstraintViolation
                        && sqlite_err.extended_code == SQLITE_CONSTRAINT_FOREIGNKEY => {}
                other => prop_assert!(
                    false,
                    "expected SQLITE_CONSTRAINT_FOREIGNKEY for unknown node, got: {other}"
                ),
            }
        }

        /// **VM CRUD round-trip**: insert + get_vm returns a row whose
        /// scalar fields match the inserted ones.
        #[test]
        fn vm_insert_then_get_returns_same_row(
            node_id in "[a-z0-9-]{1,8}",
            vm_id in "[a-z0-9-]{1,8}",
            name in "[a-z0-9-]{1,12}",
            cpu in 1i32..=64,
            memory_bytes in 1i64..(1i64 << 40),
            auto_start in any::<bool>(),
        ) {
            let db = Database::open(":memory:").expect("open db");
            db.upsert_node(&make_node(
                &node_id, "h", "127.0.0.1:9091",
                4, 1024, 0, 0, false, 0, "DC1",
            )).unwrap();
            let vm = make_vm(&vm_id, &name, &node_id, cpu, memory_bytes, auto_start);
            db.insert_vm(&vm).unwrap();

            let got = db.get_vm(&vm_id).unwrap().expect("vm exists");
            prop_assert_eq!(&got.id, &vm.id);
            prop_assert_eq!(&got.name, &vm.name);
            prop_assert_eq!(got.cpu, vm.cpu);
            prop_assert_eq!(got.memory_bytes, vm.memory_bytes);
            prop_assert_eq!(got.auto_start, vm.auto_start);
            prop_assert_eq!(&got.node_id, &vm.node_id);
        }

        /// **Delete consistency**: after `delete_vm_by_id_or_name`, the
        /// VM disappears from every observable view.
        #[test]
        fn vm_delete_removes_from_all_views(
            node_id in "[a-z0-9-]{1,8}",
            vm_id in "[a-z0-9-]{1,8}",
        ) {
            let db = Database::open(":memory:").expect("open db");
            db.upsert_node(&make_node(
                &node_id, "h", "127.0.0.1:9091",
                4, 1024, 0, 0, false, 0, "DC1",
            )).unwrap();
            // Use the same string for id and name so deletion by either
            // matches the row.
            let vm = make_vm(&vm_id, &vm_id, &node_id, 1, 1024, false);
            db.insert_vm(&vm).unwrap();
            prop_assert!(db.get_vm(&vm_id).unwrap().is_some());

            let deleted = db.delete_vm_by_id_or_name(&vm_id).unwrap();
            prop_assert!(deleted);
            prop_assert!(db.get_vm(&vm_id).unwrap().is_none());
            prop_assert!(db.find_node_for_vm(&vm_id).unwrap().is_none());
            let listed = db.list_vms().unwrap();
            prop_assert!(
                !listed.iter().any(|v| v.id == vm_id),
                "deleted vm {vm_id:?} still appears in list_vms()"
            );
        }

        /// **Heartbeat idempotence (modulo timestamp)**: calling
        /// `update_heartbeat` twice with identical arguments produces
        /// the same node state in every field except `last_heartbeat`
        /// (which is `datetime('now')` and may shift between calls).
        ///
        /// This is the invariant that protects the scheduler from
        /// flapping resource reservations on a quiet, healthy node.
        #[test]
        fn heartbeat_is_idempotent_modulo_timestamp(
            node_id in "[a-z0-9-]{1,8}",
            cpu_used in 0i32..=64,
            mem_used in 0i64..(1i64 << 36),
            cert_days in -1i32..=3650,
            luks in prop::sample::select(vec!["", "tpm", "passphrase"]),
        ) {
            let db = Database::open(":memory:").expect("open db");
            db.upsert_node(&make_node(
                &node_id, "h", "127.0.0.1:9091",
                4, 1024, 0, 0, false, 0, "DC1",
            )).unwrap();

            db.update_heartbeat(&node_id, cpu_used, mem_used, cert_days, luks).unwrap();
            let after_first = db.get_node(&node_id).unwrap().expect("node");
            db.update_heartbeat(&node_id, cpu_used, mem_used, cert_days, luks).unwrap();
            let after_second = db.get_node(&node_id).unwrap().expect("node");

            prop_assert_eq!(after_first.cpu_used, after_second.cpu_used);
            prop_assert_eq!(after_first.memory_used, after_second.memory_used);
            prop_assert_eq!(after_first.cert_expiry_days, after_second.cert_expiry_days);
            prop_assert_eq!(&after_first.luks_method, &after_second.luks_method);
            prop_assert_eq!(&after_first.status, &after_second.status);
            prop_assert_eq!(&after_first.approval_status, &after_second.approval_status);
        }

        /// **DiskLayout CRUD round-trip**: for any randomised
        /// `DiskLayoutRow` whose `node_id` matches an existing node,
        /// `upsert_disk_layout` followed by `get_disk_layout` returns
        /// a row whose user-set fields equal the inserted ones.
        #[test]
        fn disk_layout_upsert_then_get_returns_same_row(
            node_id in "[a-z0-9-]{1,8}",
            name in "[a-z0-9-]{1,12}",
            generation in 1i64..=1024,
            layout_nix in "[ -~\n]{0,256}",
        ) {
            let db = Database::open(":memory:").expect("open db");
            db.upsert_node(&make_node(
                &node_id, "h", "127.0.0.1:9091",
                4, 1024, 0, 0, false, 0, "DC1",
            )).unwrap();

            let layout = DiskLayoutRow {
                name: name.clone(),
                node_id: node_id.clone(),
                generation,
                layout_nix: layout_nix.clone(),
                evacuate: false,
                created_at: String::new(),
                updated_at: String::new(),
            };
            db.upsert_disk_layout(&layout).unwrap();

            let got = db.get_disk_layout(&name).unwrap().expect("layout exists");
            prop_assert_eq!(&got.name, &name);
            prop_assert_eq!(&got.node_id, &node_id);
            prop_assert_eq!(got.generation, generation);
            prop_assert_eq!(&got.layout_nix, &layout_nix);
            prop_assert!(!got.evacuate);
            prop_assert!(!got.created_at.is_empty());
            prop_assert!(!got.updated_at.is_empty());
        }

        /// **DiskLayout evacuate flag** persists through upsert/get.
        #[test]
        fn disk_layout_evacuate_flag_round_trips(node_id in "[a-z0-9-]{1,8}", name in "[a-z0-9-]{1,12}") {
            let db = Database::open(":memory:").expect("open db");
            db.upsert_node(&make_node(
                &node_id, "h", "127.0.0.1:9091",
                4, 1024, 0, 0, false, 0, "DC1",
            )).unwrap();
            let layout = DiskLayoutRow {
                name: name.clone(),
                node_id: node_id.clone(),
                generation: 1,
                layout_nix: "{ disko.devices = {}; }".to_string(),
                evacuate: true,
                created_at: String::new(),
                updated_at: String::new(),
            };
            db.upsert_disk_layout(&layout).unwrap();
            let got = db.get_disk_layout(&name).unwrap().expect("layout exists");
            prop_assert!(got.evacuate);
        }

        /// **DiskLayout upsert is idempotent**: applying the same row
        /// twice yields exactly one row in `list_disk_layouts`.
        #[test]
        fn disk_layout_upsert_is_idempotent(
            node_id in "[a-z0-9-]{1,8}",
            name in "[a-z0-9-]{1,12}",
        ) {
            let db = Database::open(":memory:").expect("open db");
            db.upsert_node(&make_node(
                &node_id, "h", "127.0.0.1:9091",
                4, 1024, 0, 0, false, 0, "DC1",
            )).unwrap();

            let layout = DiskLayoutRow {
                name: name.clone(),
                node_id: node_id.clone(),
                generation: 1,
                layout_nix: "{ disko.devices = {}; }".to_string(),
                evacuate: false,
                created_at: String::new(),
                updated_at: String::new(),
            };
            db.upsert_disk_layout(&layout).unwrap();
            db.upsert_disk_layout(&layout).unwrap();

            let listed = db.list_disk_layouts(None).unwrap();
            prop_assert_eq!(listed.len(), 1);
            prop_assert_eq!(&listed[0].name, &name);

            let only_node = db.list_disk_layouts(Some(&node_id)).unwrap();
            prop_assert_eq!(only_node.len(), 1);
        }

        /// **DiskLayout FK integrity**: inserting a layout whose
        /// `node_id` does not exist must fail with a SQLite FK
        /// constraint violation.
        #[test]
        fn disk_layout_rejects_unknown_node_id(
            present_node in "[a-z0-9-]{1,8}",
            missing_node in "[a-z0-9-]{1,8}",
            name in "[a-z0-9-]{1,12}",
        ) {
            prop_assume!(present_node != missing_node);
            let db = Database::open(":memory:").expect("open db");
            db.upsert_node(&make_node(
                &present_node, "h", "127.0.0.1:9091",
                1, 1024, 0, 0, false, 0, "DC1",
            )).unwrap();

            let layout = DiskLayoutRow {
                name,
                node_id: missing_node,
                generation: 1,
                layout_nix: "{}".to_string(),
                evacuate: false,
                created_at: String::new(),
                updated_at: String::new(),
            };
            const SQLITE_CONSTRAINT_FOREIGNKEY: i32 = 787;
            match db.upsert_disk_layout(&layout) {
                Ok(_) => prop_assert!(false, "expected FK violation, got Ok"),
                Err(rusqlite::Error::SqliteFailure(sqlite_err, _))
                    if sqlite_err.code == rusqlite::ErrorCode::ConstraintViolation
                        && sqlite_err.extended_code == SQLITE_CONSTRAINT_FOREIGNKEY => {}
                Err(other) => prop_assert!(
                    false,
                    "expected SQLITE_CONSTRAINT_FOREIGNKEY, got: {other}"
                ),
            }
        }

        /// **DiskLayout delete removes from every view**: after
        /// `delete_disk_layout`, the layout disappears from
        /// `get_disk_layout` and `list_disk_layouts` and any status
        /// row is cascaded away.
        #[test]
        fn disk_layout_delete_removes_from_all_views(
            node_id in "[a-z0-9-]{1,8}",
            name in "[a-z0-9-]{1,12}",
        ) {
            let db = Database::open(":memory:").expect("open db");
            db.upsert_node(&make_node(
                &node_id, "h", "127.0.0.1:9091",
                4, 1024, 0, 0, false, 0, "DC1",
            )).unwrap();

            let layout = DiskLayoutRow {
                name: name.clone(),
                node_id: node_id.clone(),
                generation: 7,
                layout_nix: "{}".to_string(),
                evacuate: false,
                created_at: String::new(),
                updated_at: String::new(),
            };
            db.upsert_disk_layout(&layout).unwrap();
            db.upsert_disk_layout_status(&DiskLayoutStatusRow {
                name: name.clone(),
                observed_generation: 7,
                phase: "applied".to_string(),
                refusal_reason: String::new(),
                message: "ok".to_string(),
                last_transition_at: String::new(),
            }).unwrap();

            let deleted = db.delete_disk_layout(&name).unwrap();
            prop_assert!(deleted);
            prop_assert!(db.get_disk_layout(&name).unwrap().is_none());
            prop_assert!(db.get_disk_layout_status(&name).unwrap().is_none());
            let listed = db.list_disk_layouts(None).unwrap();
            prop_assert!(!listed.iter().any(|l| l.name == name));
        }

        /// **Reconciler queue invariant**: a layout shows up in
        /// `list_disk_layouts_needing_reconcile` iff it has no status
        /// row, or its status `observed_generation` is strictly
        /// behind the layout's `generation`. Once status catches up,
        /// it must drop out of the queue.
        #[test]
        fn disk_layout_reconcile_queue_tracks_observed_generation(
            node_id in "[a-z0-9-]{1,8}",
            name in "[a-z0-9-]{1,12}",
            generation in 1i64..=64,
        ) {
            let db = Database::open(":memory:").expect("open db");
            db.upsert_node(&make_node(
                &node_id, "h", "127.0.0.1:9091",
                4, 1024, 0, 0, false, 0, "DC1",
            )).unwrap();

            db.upsert_disk_layout(&DiskLayoutRow {
                name: name.clone(),
                node_id: node_id.clone(),
                generation,
                layout_nix: "{}".to_string(),
                evacuate: false,
                created_at: String::new(),
                updated_at: String::new(),
            }).unwrap();

            let pending = db.list_disk_layouts_needing_reconcile().unwrap();
            prop_assert!(pending.iter().any(|l| l.name == name),
                "freshly created layout must be queued for reconcile");

            db.upsert_disk_layout_status(&DiskLayoutStatusRow {
                name: name.clone(),
                observed_generation: generation,
                phase: "applied".to_string(),
                refusal_reason: String::new(),
                message: String::new(),
                last_transition_at: String::new(),
            }).unwrap();
            let after = db.list_disk_layouts_needing_reconcile().unwrap();
            prop_assert!(!after.iter().any(|l| l.name == name),
                "layout with observed_generation == generation must NOT be queued");

            db.upsert_disk_layout(&DiskLayoutRow {
                name: name.clone(),
                node_id: node_id.clone(),
                generation: generation + 1,
                layout_nix: "{ updated = true; }".to_string(),
                evacuate: false,
                created_at: String::new(),
                updated_at: String::new(),
            }).unwrap();
            let bumped = db.list_disk_layouts_needing_reconcile().unwrap();
            prop_assert!(bumped.iter().any(|l| l.name == name),
                "layout whose generation moved past observed_generation must be re-queued");
        }

        /// **CephCluster reconciler queue**: same observed_generation
        /// invariant as disk layouts — null status or lagging generation
        /// must appear in `list_ceph_clusters_needing_reconcile`.
        #[test]
        fn ceph_cluster_reconcile_queue_tracks_observed_generation(
            name in "[a-z0-9-]{1,12}",
            generation in 1i64..=64,
        ) {
            let db = Database::open(":memory:").expect("open db");
            db.upsert_ceph_cluster(&CephClusterRow {
                name: name.clone(),
                generation,
                spec_json: "{}".to_string(),
                bootstrap_json: String::new(),
                created_at: String::new(),
                updated_at: String::new(),
            }).unwrap();

            let pending = db.list_ceph_clusters_needing_reconcile().unwrap();
            prop_assert!(pending.iter().any(|c| c.name == name),
                "fresh CephCluster must be queued for reconcile");

            db.upsert_ceph_cluster_status(&CephClusterStatusRow {
                name: name.clone(),
                observed_generation: generation,
                phase: "healthy".to_string(),
                health_message: String::new(),
                ceph_status_json: String::new(),
                last_transition_at: String::new(),
            }).unwrap();
            let after = db.list_ceph_clusters_needing_reconcile().unwrap();
            prop_assert!(!after.iter().any(|c| c.name == name),
                "caught-up CephCluster must NOT be queued");

            db.upsert_ceph_cluster(&CephClusterRow {
                name: name.clone(),
                generation: generation + 1,
                spec_json: "{\"updated\":true}".to_string(),
                bootstrap_json: String::new(),
                created_at: String::new(),
                updated_at: String::new(),
            }).unwrap();
            let bumped = db.list_ceph_clusters_needing_reconcile().unwrap();
            prop_assert!(bumped.iter().any(|c| c.name == name),
                "CephCluster whose generation moved past observed_generation must be re-queued");
        }

        /// **Operator create/list**: `create_operator` + `list_operator_rows` round-trip.
        #[test]
        fn operator_create_then_list_contains_row(
            name in "[a-z][a-z0-9_-]{0,31}",
        ) {
            let db = Database::open(":memory:").expect("open db");
            db.create_operator(&name).unwrap();
            let rows = db.list_operator_rows().unwrap();
            prop_assert!(rows.iter().any(|r| r.name == name));
            prop_assert_eq!(db.count_operators().unwrap(), 1);
        }

        /// **`grant_operator_role_str` idempotence**: repeated grants keep a single logical row.
        #[test]
        fn grant_operator_role_str_is_idempotent(
            name in "[a-z][a-z0-9_-]{0,31}",
        ) {
            let db = Database::open(":memory:").expect("open db");
            db.create_operator(&name).unwrap();
            db.grant_operator_role_str(&name, "vm-admin").unwrap();
            db.grant_operator_role_str(&name, "vm-admin").unwrap();
            let roles = db.list_operator_role_strings(&name).unwrap();
            prop_assert_eq!(roles.iter().filter(|r| *r == "vm-admin").count(), 1);
        }

        /// **`delete_operator` cascades** roles (no orphan role rows).
        #[test]
        fn delete_operator_cascades_roles(
            name in "[a-z][a-z0-9_-]{0,31}",
        ) {
            let db = Database::open(":memory:").expect("open db");
            db.create_operator(&name).unwrap();
            db.grant_operator_role_str(&name, "read-only").unwrap();
            prop_assert!(db.delete_operator(&name).unwrap());
            prop_assert!(db.list_operator_role_strings(&name).unwrap().is_empty());
            prop_assert!(db.get_operator_row(&name).unwrap().is_none());
        }
    }

    #[test]
    fn snap_clone_parent_link_blocks_prune_semantics() {
        let db = Database::open(":memory:").expect("db");
        let mut root = VolumeRow::new_data("pgdata", 10 * 1024 * 1024 * 1024);
        root.vm_id = "vm-1".into();
        root.attach_state = VolumeRow::ATTACH_ATTACHED.into();
        db.upsert_volume(&root).unwrap();
        let snap = VolumeSnapshotRow {
            id: "snap-1".into(),
            name: "pgdata-t0".into(),
            volume_id: root.id.clone(),
            rbd_snap: "s1".into(),
            protected: true,
            size_bytes: root.size_bytes,
            consistency: "crash".into(),
            created_at: String::new(),
        };
        db.insert_volume_snapshot(&snap).unwrap();
        let mut clone = VolumeRow::new_data("pgdata-clone", root.size_bytes);
        clone.parent_snapshot_id = snap.id.clone();
        db.upsert_volume(&clone).unwrap();
        assert_eq!(db.count_volumes_with_parent_snapshot(&snap.id).unwrap(), 1);
        clone.parent_snapshot_id.clear();
        db.upsert_volume(&clone).unwrap();
        assert_eq!(db.count_volumes_with_parent_snapshot(&snap.id).unwrap(), 0);
    }

    #[test]
    fn encrypted_volume_persists_wrapped_dek() {
        let db = Database::open(":memory:").expect("db");
        let mut vol = VolumeRow::new_data("secret", 1024 * 1024);
        vol.encrypted = true;
        vol.wrapped_dek = "v1blob".into();
        db.upsert_volume(&vol).unwrap();
        let loaded = db.get_volume_by_name("secret").unwrap().unwrap();
        assert!(loaded.encrypted);
        assert_eq!(loaded.wrapped_dek, "v1blob");
    }

    #[test]
    fn guest_ops_node_keys_round_trip() {
        let db = Database::open(":memory:").expect("db");
        db.upsert_guest_ops_key("n1", "ssh-ed25519 AAAA n1")
            .unwrap();
        let keys = db.list_guest_ops_keys().unwrap();
        assert_eq!(keys.len(), 1);
        assert_eq!(keys[0].0, "n1");
    }
}
