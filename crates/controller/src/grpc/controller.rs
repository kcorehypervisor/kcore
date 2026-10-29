mod ingress;

use std::cell::RefCell;
use std::collections::HashSet;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::time::Duration;

thread_local! {
    // Nested RPC helpers (CreateWorkload → CreateVm) lose mTLS extensions on
    // Request::new; propagate the outer peer CN for audit attribution.
    static AUDIT_ACTOR_OVERRIDE: RefCell<Option<String>> = const { RefCell::new(None) };
}

struct AuditActorGuard;

impl Drop for AuditActorGuard {
    fn drop(&mut self) {
        AUDIT_ACTOR_OVERRIDE.with(|c| {
            *c.borrow_mut() = None;
        });
    }
}
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;
use tokio_stream::Stream;
use tonic::{Request, Response, Status};
use tracing::{debug, error, info, warn};
use uuid::Uuid;

use crate::auth::{self, OperatorRole, CN_NODE_PREFIX};
use crate::ceph_cluster_spec;
use crate::cluster_update_spec;
use crate::config::{NetworkConfig, ReplicationConfig};
use crate::controller_proto;
use crate::db::{
    CephClusterRow, CephClusterStatusRow, ClusterUpdateNodeRow, ClusterUpdateRow, Database,
    DiskLayoutRow, DiskLayoutStatusRow, NetworkRow, NodeRow, ObjectStoreRow, ObjectStoreStatusRow,
    ObjectUserRow, OperatorRow, PostgresqlRow, SecurityGroupRow, SecurityGroupRuleRow,
    SharedFilesystemRow, SharedFilesystemStatusRow, SlurmClusterNodeRow, SlurmClusterRow,
    SnapshotPolicyRow, VmNicRow, VmOperationRow, VmRow, VolumeRow, VolumeSnapshotRow, WorkloadRow,
};
#[cfg(test)]
use crate::db::{IssuedCertRow, CERT_STATUS_ACTIVE, CERT_STATUS_REVOKED};
use crate::node_proto;
use crate::object_store_spec;
use crate::shared_filesystem_spec;
use crate::volume_snapshot;
use crate::{nixgen, node_client::NodeClients, scheduler};
use kcore_sanitize::sanitize_nix_attr_key;
use std::collections::HashMap;

use super::helpers::compute_vni;
use super::helpers::{
    controller_state_from_node_state, grpc_address_host, migration_dial_host,
    parse_datetime_to_timestamp, parse_port_list, receive_state_from_node, short_vm_id_seed,
    state_fallback_without_runtime, status_with_context, vm_backend_handle,
};

fn chrono_like_now() -> String {
    // RFC3339-ish UTC from unix seconds; guest_checked_at is informational.
    use std::time::{SystemTime, UNIX_EPOCH};
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    format!("unix:{secs}")
}

/// Virtio serial the guest sees for this volume.
/// Ceph root disks use `serial=root-<runtimeName>` in ch-vm; data disks use
/// `VolumeRow.serial`.
fn guest_virtio_serial(vol: &VolumeRow, runtime_vm_name: &str) -> String {
    if vol.role == VolumeRow::ROLE_ROOT {
        format!("root-{runtime_vm_name}")
    } else {
        vol.serial.clone()
    }
}

#[cfg(test)]
mod guest_serial_tests {
    use super::*;

    #[test]
    fn root_uses_ch_vm_root_prefix() {
        let mut vol = VolumeRow::new_root("vm-1", "app-1", 10 * 1024 * 1024 * 1024);
        vol.serial = "deadbeef".into();
        assert_eq!(guest_virtio_serial(&vol, "app-1"), "root-app-1");
    }

    #[test]
    fn data_uses_volume_serial() {
        let mut vol = VolumeRow::new_data("pgdata", 1024);
        vol.serial = "abcdef012345".into();
        assert_eq!(guest_virtio_serial(&vol, "app-1"), "abcdef012345");
    }
}

/// Shallow-merge `extra` object keys into `base` JSON object (both may be empty).
fn merge_detail_json(base: &str, extra: &str) -> String {
    let mut map = match serde_json::from_str::<serde_json::Map<String, serde_json::Value>>(
        if base.trim().is_empty() { "{}" } else { base },
    ) {
        Ok(m) => m,
        Err(_) => serde_json::Map::new(),
    };
    if let Ok(extra_map) = serde_json::from_str::<serde_json::Map<String, serde_json::Value>>(extra)
    {
        for (k, v) in extra_map {
            map.insert(k, v);
        }
    }
    serde_json::Value::Object(map).to_string()
}

fn vm_labels(raw: &str) -> std::collections::HashMap<String, String> {
    serde_json::from_str(raw).unwrap_or_default()
}

fn restore_labelled(raw: &str) -> bool {
    vm_labels(raw).contains_key("kcore-vault/restore")
}
use super::rbac_matrix;
use super::signing;
use super::validation::{
    derive_image_format, derive_image_format_from_path, derive_local_image_path,
    image_used_in_place, normalize_image_format, normalize_storage_backend,
    storage_backend_to_proto, validate_image_path, validate_image_sha256, validate_image_url,
    validate_ipv4, validate_netmask, validate_network_name, validate_network_type,
    validate_postgresql_database, validate_postgresql_name, validate_postgresql_package,
    validate_postgresql_port, validate_storage_size_bytes, vault_boot_volume_name,
    VAULT_BOOT_IMAGE,
};

#[cfg(test)]
type PushHook = std::sync::Arc<dyn Fn(&NodeRow) -> Result<(), Status> + Send + Sync + 'static>;
/// Test stand-in for `NodeStorage.FenceVolume`: (backend_handle, source_hosts).
#[cfg(test)]
type FenceHook =
    std::sync::Arc<dyn Fn(&str, &[String]) -> Result<Vec<String>, Status> + Send + Sync + 'static>;

/// Live migrate failed; `send_succeeded` means the guest may already run on dest
/// and cold fallback must not start a second VMM on the shared RBD.
struct LiveMigrateFailure {
    send_succeeded: bool,
    status: Status,
    operation_id: String,
}

/// A finished live migration plus what the destination wanted the operator
/// to know (host differences it tolerated, e.g. a TSC rate change).
struct LiveMigrateOutcome {
    operation_id: String,
    warnings: Vec<String>,
}

/// Whether a node may be handed a VM that is moving off another node.
///
/// `scheduler::select_node_for_vm` applies these rules when it picks a target,
/// but an operator-supplied `target_node` used to bypass them entirely — so
/// `MigrateVm` and `DrainNode` would happily move a guest onto a node that was
/// itself being evacuated, or one that has not been approved into the cluster.
#[allow(clippy::result_large_err)]
fn gpu_info_from_row(
    row: crate::db::NodeGpuRow,
    assigned: &std::collections::HashMap<(String, String), String>,
) -> controller_proto::GpuInfo {
    let assigned_vm = assigned
        .get(&(row.node_id.clone(), row.name.clone()))
        .cloned()
        .unwrap_or_default();
    let kind = if row.kind.is_empty() {
        "gpu".into()
    } else {
        row.kind
    };
    controller_proto::GpuInfo {
        name: row.name,
        family: row.family,
        model: row.model,
        address: row.address,
        pci_devices: crate::pci::split_pci_devices(&row.pci_devices),
        iommu_group: row.iommu_group,
        assignable: row.assignable,
        blocked_reason: row.blocked_reason,
        node_id: row.node_id,
        assigned_vm,
        kind,
        role: row.role,
        class_code: row.class_code,
        characteristics: row.characteristics,
        driver: row.driver,
        parent: row.parent,
        profile: row.profile,
        profiles: row
            .profiles
            .split(',')
            .map(str::trim)
            .filter(|profile| !profile.is_empty())
            .map(str::to_string)
            .collect(),
    }
}

/// A fence counts only when it covers the lost host's address, not merely
/// a watcher that happened to still be listed. The node agent prefixes
/// those entries with `range:`.
fn fence_covers_lost_host(entries: &[String]) -> bool {
    entries.iter().any(|entry| entry.starts_with("range:"))
}

fn accepts_migrated_vms(node: &NodeRow) -> Result<(), Status> {
    if node.approval_status != "approved" {
        return Err(Status::failed_precondition(format!(
            "node '{}' is not approved ({}); it cannot be given VMs",
            node.id, node.approval_status
        )));
    }
    if node.status == "cordoned" {
        return Err(Status::failed_precondition(format!(
            "node '{}' is cordoned; it will not accept VMs until it is uncordoned",
            node.id
        )));
    }
    if matches!(node.status.as_str(), "draining" | "drained") {
        return Err(Status::failed_precondition(format!(
            "node '{}' is being evacuated (status {}); moving a VM onto it would undo the drain",
            node.id, node.status
        )));
    }
    Ok(())
}

/// Options for [`ControllerService::drain_node_vms`], shared by `DrainNode` RPC
/// and DiskLayout evacuate reconciliation.
pub struct NodeDrainOptions {
    /// When true, set the source node status to `draining` before moving VMs.
    pub mark_draining_at_start: bool,
    /// When true, set the source node to `drained` or `draining` after work.
    pub update_final_node_status: bool,
}

impl Default for NodeDrainOptions {
    fn default() -> Self {
        Self {
            mark_draining_at_start: true,
            update_final_node_status: true,
        }
    }
}

pub struct NodeDrainOutcome {
    pub migrated: i32,
    pub errors: Vec<String>,
}

/// Whether a node's receive-session report describes something that may still
/// be receiving a migration.
///
/// A pid that is alive but does not belong to this VM is a recycled number, not
/// a live receive, so it deliberately does not count. This is only ever used to
/// *tell the operator* — nothing in the controller reaps a session on the
/// strength of it, because a wrong guess would destroy a live migration.
fn receive_looks_live(state: &controller_proto::LiveMigrateReceiveState) -> bool {
    (state.vmm_alive && state.vmm_pid_matches_vm) || state.port_listening
}

/// What `ResetLiveMigrateReceive` should do about a node's observed state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ResetPlan {
    /// Nothing of the session is left; report success and touch nothing.
    NothingToClear,
    /// Something is there but `force` was not given: describe it and stop.
    ReportOnly,
    /// Go ahead and run the cleanup.
    Clear,
}

/// Gate the destructive path on explicit intent.
///
/// Reporting is the default so that running the command by accident, or before
/// reading the state, cannot destroy anything. A node that reports no state at
/// all is treated as having something to clear: the conservative reading is
/// that leftovers exist, and the operator has already asked for `force`.
fn plan_reset(
    observed: Option<&controller_proto::LiveMigrateReceiveState>,
    force: bool,
) -> ResetPlan {
    let nothing_to_clear = observed.is_some_and(|s| {
        !s.has_session && !s.pid_file_present && !s.marker_present && !s.api_socket_present
    });
    if nothing_to_clear {
        ResetPlan::NothingToClear
    } else if force {
        ResetPlan::Clear
    } else {
        ResetPlan::ReportOnly
    }
}

/// Decide whether a node's `FinalizeLiveMigrateSource` reply proves the source
/// has really let go of the shared RBD image.
///
/// The node answers with what it *observed* — the unit is inactive, the device
/// is no longer mapped — rather than with the fact that it issued the calls. A
/// reply whose post-conditions are false means the guest or the mapping is
/// still there, so whatever was going to touch that image next must not.
#[allow(clippy::result_large_err)]
fn check_release_barrier(
    vm_name: &str,
    node_id: &str,
    resp: &node_proto::FinalizeLiveMigrateSourceResponse,
) -> Result<(), Status> {
    if resp.vmm_stopped && resp.rbd_unmapped {
        return Ok(());
    }
    Err(Status::failed_precondition(format!(
        "node {node_id} did not release VM '{vm_name}' (vmm_stopped={}, rbd_unmapped={}): {}; \
         refusing to touch the shared image while the source may still be writing",
        resp.vmm_stopped, resp.rbd_unmapped, resp.message
    )))
}

/// What a single `GetNixApplyStatus` poll tells the caller to do next.
#[derive(Debug, PartialEq, Eq)]
enum NixApplyProgress {
    /// The rebuild activated; the new configuration is live.
    Activated,
    /// Still building. Poll again.
    Pending,
    /// The rebuild failed; the node is running its previous configuration.
    Failed,
    /// The node cannot answer for this apply (its `/run` state was discarded,
    /// or a newer apply superseded it). Polling longer cannot help.
    NoVerdict,
}

fn nix_apply_progress(phase: i32) -> NixApplyProgress {
    match node_proto::NixApplyPhase::try_from(phase) {
        Ok(node_proto::NixApplyPhase::Succeeded) => NixApplyProgress::Activated,
        Ok(node_proto::NixApplyPhase::Failed) => NixApplyProgress::Failed,
        Ok(node_proto::NixApplyPhase::Running) => NixApplyProgress::Pending,
        Ok(node_proto::NixApplyPhase::Unknown) => NixApplyProgress::NoVerdict,
        // An unset phase is a node that has not written its first state yet.
        Ok(node_proto::NixApplyPhase::Unspecified) | Err(_) => NixApplyProgress::Pending,
    }
}

/// Upper bound on how long a caller waits for a node's `nixos-rebuild` to
/// activate. Matched to the live-migration send/receive timeouts so a migration
/// cannot be bounded by one leg and unbounded by the other. It exists to fail
/// with a clear error rather than hang; a warm node activates in well under a
/// minute.
const NIX_APPLY_WAIT_TIMEOUT: Duration = Duration::from_secs(600);
const NIX_APPLY_POLL_INTERVAL: Duration = Duration::from_secs(2);
const EVT_NODE_REGISTER: &str = "node.register";
const EVT_NODE_HEARTBEAT: &str = "node.heartbeat";
const EVT_NODE_APPROVE: &str = "node.approve";
const EVT_NODE_REJECT: &str = "node.reject";
const EVT_VM_CREATE: &str = "vm.create";
const EVT_VM_UPDATE: &str = "vm.update";
const EVT_VM_DELETE: &str = "vm.delete";
const EVT_VM_DESIRED_STATE_SET: &str = "vm.desired_state.set";
const EVT_NETWORK_CREATE: &str = "network.create";
const EVT_NETWORK_DELETE: &str = "network.delete";
const EVT_POSTGRESQL_CREATE: &str = "postgresql.create";
const EVT_POSTGRESQL_UPDATE: &str = "postgresql.update";
const EVT_POSTGRESQL_DELETE: &str = "postgresql.delete";
const EVT_SLURM_CLUSTER_CREATE: &str = "slurm_cluster.create";
const EVT_SLURM_CLUSTER_UPDATE: &str = "slurm_cluster.update";
const EVT_SLURM_CLUSTER_DELETE: &str = "slurm_cluster.delete";
const EVT_SECURITY_GROUP_CREATE: &str = "security_group.create";
const EVT_SECURITY_GROUP_DELETE: &str = "security_group.delete";
const EVT_SECURITY_GROUP_ATTACH: &str = "security_group.attach";
const EVT_SECURITY_GROUP_DETACH: &str = "security_group.detach";
const EVT_NODE_DRAIN: &str = "node.drain";
const EVT_NODE_CORDON: &str = "node.cordon";
const EVT_NODE_UNCORDON: &str = "node.uncordon";
const EVT_NODE_DELETE: &str = "node.delete";
const EVT_VM_MIGRATE: &str = "vm.migrate";
const EVT_SSH_KEY_CREATE: &str = "ssh_key.create";
const EVT_SSH_KEY_DELETE: &str = "ssh_key.delete";
const EVT_VOLUME_CREATE: &str = "volume.create";
const EVT_VOLUME_UPDATE: &str = "volume.update";
const EVT_VOLUME_DELETE: &str = "volume.delete";
const EVT_VOLUME_ATTACH: &str = "volume.attach";
const EVT_VOLUME_DETACH: &str = "volume.detach";
const EVT_VOLUME_ENCRYPT: &str = "volume.encrypt";
const EVT_VOLUME_SNAPSHOT_CREATE: &str = "volume_snapshot.create";
const EVT_VOLUME_SNAPSHOT_DELETE: &str = "volume_snapshot.delete";
const EVT_DISK_LAYOUT_CREATE: &str = "disk_layout.create";
const EVT_DISK_LAYOUT_DELETE: &str = "disk_layout.delete";
const EVT_CLUSTER_UPDATE_CREATE: &str = "cluster_update.create";
const EVT_CLUSTER_UPDATE_APPROVE: &str = "cluster_update.approve";
const EVT_CLUSTER_UPDATE_CANCEL: &str = "cluster_update.cancel";
const EVT_CLUSTER_UPDATE_ROLLBACK: &str = "cluster_update.rollback";
const EVT_OPERATOR_UPSERT: &str = "operator.upsert";
const EVT_OPERATOR_DELETE: &str = "operator.delete";
const EVT_OPERATOR_ROLE_GRANT: &str = "operator_role.grant";
const EVT_OPERATOR_ROLE_REVOKE: &str = "operator_role.revoke";

fn normalize_sg_protocol(protocol: &str) -> Result<String, Status> {
    let p = protocol.trim().to_ascii_lowercase();
    match p.as_str() {
        "tcp" | "udp" => Ok(p),
        _ => Err(Status::invalid_argument(
            "security group rule protocol must be tcp or udp",
        )),
    }
}

/// Map a VmRow.runtime_state string back into the proto VmState enum value.
fn vm_state_from_runtime_str(s: &str) -> i32 {
    match s {
        "running" => controller_proto::VmState::Running as i32,
        "stopped" => controller_proto::VmState::Stopped as i32,
        "paused" => controller_proto::VmState::Paused as i32,
        "error" => controller_proto::VmState::Error as i32,
        _ => controller_proto::VmState::Unknown as i32,
    }
}

fn validate_port(port: i32, field: &str) -> Result<i32, Status> {
    if (1..=65535).contains(&port) {
        Ok(port)
    } else {
        Err(Status::invalid_argument(format!(
            "{field} must be in range 1-65535"
        )))
    }
}

fn parse_sg_target_kind(kind: i32) -> Result<controller_proto::SecurityGroupTargetKind, Status> {
    let parsed = controller_proto::SecurityGroupTargetKind::try_from(kind)
        .unwrap_or(controller_proto::SecurityGroupTargetKind::Unspecified);
    match parsed {
        controller_proto::SecurityGroupTargetKind::Vm
        | controller_proto::SecurityGroupTargetKind::Network => Ok(parsed),
        controller_proto::SecurityGroupTargetKind::Unspecified => Err(Status::invalid_argument(
            "target_kind must be vm or network",
        )),
    }
}

#[derive(Clone, Default)]
pub struct SubCaState {
    pub cert_pem: String,
    pub key_pem: String,
    pub cert_file: Option<String>,
    pub key_file: Option<String>,
}

impl SubCaState {
    pub fn is_available(&self) -> bool {
        !self.cert_pem.is_empty() && !self.key_pem.is_empty()
    }
}

#[derive(Clone, Default)]
pub struct TlsPaths {
    pub cert_file: String,
    pub key_file: String,
}

/// PKI state and policy shared with the rotation reconciler and the HTTP
/// responder. Defaults are inert so tests that do not care about PKI can
/// construct the service unchanged.
#[derive(Clone)]
pub struct PkiRuntime {
    pub crl_cache: crate::pki::crl::CrlCache,
    pub revocation: crate::pki::revocation::RevocationState,
    pub rotation: crate::config::CertRotationConfig,
    pub pki: crate::config::PkiConfig,
}

impl Default for PkiRuntime {
    fn default() -> Self {
        Self {
            crl_cache: crate::pki::crl::CrlCache::new(),
            revocation: crate::pki::revocation::RevocationState::disabled(),
            rotation: crate::config::CertRotationConfig::default(),
            pki: crate::config::PkiConfig::default(),
        }
    }
}

#[derive(Clone)]
pub struct ControllerService {
    db: Database,
    clients: NodeClients,
    default_network: NetworkConfig,
    sub_ca: Arc<Mutex<SubCaState>>,
    replication: Option<ReplicationConfig>,
    tls_paths: Option<TlsPaths>,
    require_manual_approval: bool,
    /// When true, legacy `CN=kctl` keeps cluster-admin after a cluster-admin operator has a cert (escape hatch).
    bootstrap_kctl: bool,
    pki: PkiRuntime,
    rate_limit: crate::config::RateLimitConfig,
    sbom: crate::config::SbomConfig,
    revocation_fail_mode: String,
    webhooks: crate::webhooks::Dispatcher,
    overcommit: crate::scheduler::Overcommit,
    failover: crate::config::FailoverConfig,
    web_consoles: WebConsoles,
    #[cfg(test)]
    test_push_hook: Option<PushHook>,
    #[cfg(test)]
    test_fence_hook: Option<FenceHook>,
    #[cfg(test)]
    test_storage_verdict: Option<crate::ha::StorageVerdict>,
}

/// Open browser consoles. Input arrives as unary calls, so each one needs
/// the node stream that `OpenVmConsole` opened.
#[derive(Clone, Default)]
struct WebConsoles(Arc<Mutex<HashMap<String, WebConsole>>>);

struct WebConsole {
    owner: String,
    to_node: mpsc::Sender<node_proto::ConsoleMessage>,
}

const WEB_CONSOLE_MAX_INPUT: usize = 64 * 1024;

fn network_policies_by_name(
    db: &crate::db::Database,
    node_id: &str,
) -> Result<std::collections::HashMap<String, crate::db::NetworkPolicyRow>, Status> {
    let rows = db
        .list_network_policies_for_node(node_id)
        .map_err(|e| Status::internal(format!("listing network policy: {e}")))?;
    Ok(rows
        .into_iter()
        .map(|row| (row.name.clone(), row))
        .collect())
}

fn vm_ipv6_by_vm(
    db: &crate::db::Database,
    vms: &[VmRow],
) -> Result<std::collections::HashMap<String, Vec<crate::db::VmIpv6Row>>, Status> {
    let mut out = std::collections::HashMap::new();
    for vm in vms {
        let rows = db
            .list_vm_ipv6(&vm.id)
            .map_err(|e| Status::internal(format!("listing IPv6 addresses: {e}")))?;
        if !rows.is_empty() {
            out.insert(vm.id.clone(), rows);
        }
    }
    Ok(out)
}

fn explicit_net_policy(
    req: &controller_proto::CreateNetworkRequest,
) -> Result<Option<(String, String, bool)>, Status> {
    let prefix = req.ipv6_prefix.trim();
    let gateway = req.ipv6_gateway.trim();
    if prefix.is_empty() != gateway.is_empty() {
        return Err(Status::invalid_argument(
            "ipv6_prefix and ipv6_gateway must both be set, or both left empty",
        ));
    }
    if !prefix.is_empty() {
        crate::net_policy::validate_ipv6_network(prefix, gateway)
            .map_err(Status::invalid_argument)?;
    }
    if prefix.is_empty() && !req.east_west_firewall {
        return Ok(None);
    }
    Ok(Some((
        prefix.to_string(),
        gateway.to_string(),
        req.east_west_firewall,
    )))
}

/// Restores a live disk mirror if the migrate RPC is cancelled while the
/// resync is still running. A client timeout drops this future at the next
/// `.await`, and `Drop` does not await, so the cleanup is spawned.
struct LiveMirrorGuard {
    armed: bool,
    service: ControllerService,
    source: NodeRow,
    target: NodeRow,
    runtime_name: String,
    op_id: String,
}

impl LiveMirrorGuard {
    fn arm(
        service: ControllerService,
        source: NodeRow,
        target: NodeRow,
        runtime_name: String,
        op_id: String,
    ) -> Self {
        Self {
            armed: true,
            service,
            source,
            target,
            runtime_name,
            op_id,
        }
    }

    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for LiveMirrorGuard {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        self.armed = false;
        let service = self.service.clone();
        let source = self.source.clone();
        let target = self.target.clone();
        let runtime_name = self.runtime_name.clone();
        let op_id = self.op_id.clone();
        let Ok(handle) = tokio::runtime::Handle::try_current() else {
            return;
        };
        handle.spawn(async move {
            warn!(
                op = %op_id,
                vm = %runtime_name,
                "live mirror RPC dropped; restoring the source disk"
            );
            match service.abort_volume_on(&source, &runtime_name, false).await {
                Ok(()) => {
                    let _ = service.abort_volume_on(&target, &runtime_name, true).await;
                    service.close_local_op(&op_id, VmOperationRow::PHASE_CANCELLED);
                }
                Err(status) => {
                    warn!(
                        op = %op_id,
                        error = %status,
                        "dropped mirror could not restore the source; destination copy kept"
                    );
                }
            }
        });
    }
}

fn extra_nics_by_vm(
    db: &Database,
    vms: &[VmRow],
) -> std::collections::HashMap<String, Vec<VmNicRow>> {
    let mut out = std::collections::HashMap::new();
    for vm in vms {
        if let Ok(nics) = db.list_vm_nics(&vm.id) {
            if !nics.is_empty() {
                out.insert(vm.id.clone(), nics);
            }
        }
    }
    out
}

impl ControllerService {
    fn assign_ipv6_if_configured(
        &self,
        node_id: &str,
        network: &str,
        vm_id: &str,
        position: i32,
    ) -> Result<(), Status> {
        let policy = self
            .db
            .get_network_policy(node_id, network)
            .map_err(|e| Status::internal(format!("reading network policy: {e}")))?;
        if policy.ipv6_prefix.is_empty() {
            return Ok(());
        }
        let global = self
            .db
            .get_network_for_node(node_id, network)
            .map_err(|e| Status::internal(format!("reading network: {e}")))?
            .map(|net| net.network_type == "vxlan")
            .unwrap_or(false);
        self.db
            .allocate_vm_ipv6(node_id, network, vm_id, position, global)
            .map_err(|e| Status::internal(format!("allocating IPv6 address: {e}")))?;
        Ok(())
    }

    fn allocate_nic_address(
        &self,
        node_id: &str,
        network: &str,
        vm_id: &str,
    ) -> Result<String, Status> {
        if network == "default" {
            return self.reserve_nat_vm_ip_for_network(
                node_id,
                network,
                vm_id,
                &self.default_network.gateway_ip,
            );
        }
        let Some(net) = self
            .db
            .get_network_for_node(node_id, network)
            .map_err(|e| Status::internal(format!("fetching network: {e}")))?
        else {
            return Ok(String::new());
        };
        match net.network_type.as_str() {
            "vxlan" => self
                .db
                .allocate_vm_ip_global(network)
                .map_err(|e| Status::internal(format!("allocating VM IP: {e}"))),
            "nat" => self.reserve_nat_vm_ip_for_network(node_id, network, vm_id, &net.gateway_ip),
            _ => Ok(String::new()),
        }
    }

    fn reserve_nat_vm_ip_for_network(
        &self,
        node_id: &str,
        vm_network: &str,
        vm_id: &str,
        gateway_ip: &str,
    ) -> Result<String, Status> {
        let mut octets = gateway_ip.split('.');
        let o0 = octets
            .next()
            .ok_or_else(|| Status::internal(format!("invalid gateway IP '{}'", gateway_ip)))?;
        let o1 = octets
            .next()
            .ok_or_else(|| Status::internal(format!("invalid gateway IP '{}'", gateway_ip)))?;
        let o2 = octets
            .next()
            .ok_or_else(|| Status::internal(format!("invalid gateway IP '{}'", gateway_ip)))?;
        let _o3 = octets
            .next()
            .ok_or_else(|| Status::internal(format!("invalid gateway IP '{}'", gateway_ip)))?;
        let prefix = format!("{o0}.{o1}.{o2}");

        let existing = self
            .db
            .list_vms_for_node(node_id)
            .map_err(|e| Status::internal(format!("listing vms for IP reservation: {e}")))?;
        let mut used_hosts: HashSet<u16> = HashSet::new();
        for vm in existing {
            if vm.network != vm_network || vm.vm_ip.is_empty() {
                continue;
            }
            let mut vm_octets = vm.vm_ip.split('.');
            let Some(v0) = vm_octets.next() else {
                continue;
            };
            let Some(v1) = vm_octets.next() else {
                continue;
            };
            let Some(v2) = vm_octets.next() else {
                continue;
            };
            let Some(v3) = vm_octets.next() else {
                continue;
            };
            if format!("{v0}.{v1}.{v2}") != prefix {
                continue;
            }
            if let Ok(host) = v3.parse::<u16>() {
                used_hosts.insert(host);
            }
        }
        if let Ok(ips) = self.db.list_nic_ips_on_node_network(node_id, vm_network) {
            for ip in ips {
                let mut octets = ip.split('.');
                let Some(v0) = octets.next() else { continue };
                let Some(v1) = octets.next() else { continue };
                let Some(v2) = octets.next() else { continue };
                let Some(v3) = octets.next() else { continue };
                if format!("{v0}.{v1}.{v2}") != prefix {
                    continue;
                }
                if let Ok(host) = v3.parse::<u16>() {
                    used_hosts.insert(host);
                }
            }
        }

        // Keep .1 for gateway and reserve lower addresses for infra.
        const MIN_HOST: u16 = 10;
        const MAX_HOST: u16 = 249;
        const POOL_SIZE: u16 = (MAX_HOST - MIN_HOST) + 1;

        let mut seed: u32 = 0;
        for b in vm_id.as_bytes() {
            seed = seed.wrapping_mul(131).wrapping_add(*b as u32);
        }
        let preferred_offset = (seed % POOL_SIZE as u32) as u16;

        for probe in 0..POOL_SIZE {
            let offset = (preferred_offset + probe) % POOL_SIZE;
            let host = MIN_HOST + offset;
            if !used_hosts.contains(&host) {
                return Ok(format!("{prefix}.{host}"));
            }
        }
        Err(Status::resource_exhausted(format!(
            "no free NAT reservation addresses for network '{}' on node '{}'",
            vm_network, node_id
        )))
    }

    pub fn new(
        db: Database,
        clients: NodeClients,
        default_network: NetworkConfig,
        sub_ca: Arc<Mutex<SubCaState>>,
        replication: Option<ReplicationConfig>,
        require_manual_approval: bool,
        bootstrap_kctl: bool,
    ) -> Self {
        Self {
            db,
            clients,
            default_network,
            sub_ca,
            replication,
            tls_paths: None,
            require_manual_approval,
            bootstrap_kctl,
            pki: PkiRuntime::default(),
            rate_limit: crate::config::RateLimitConfig::default(),
            sbom: crate::config::SbomConfig::default(),
            revocation_fail_mode: crate::config::RevocationConfig::default().fail_mode,
            webhooks: crate::webhooks::Dispatcher::noop(),
            overcommit: crate::scheduler::Overcommit::default(),
            failover: crate::config::FailoverConfig::default(),
            web_consoles: WebConsoles::default(),
            #[cfg(test)]
            test_push_hook: None,
            #[cfg(test)]
            test_fence_hook: None,
            #[cfg(test)]
            test_storage_verdict: None,
        }
    }

    pub fn with_tls_paths(mut self, paths: TlsPaths) -> Self {
        self.tls_paths = Some(paths);
        self
    }

    pub fn with_pki(mut self, pki: PkiRuntime) -> Self {
        self.pki = pki;
        self
    }

    pub fn with_security(
        mut self,
        rate_limit: crate::config::RateLimitConfig,
        sbom: crate::config::SbomConfig,
        revocation_fail_mode: String,
    ) -> Self {
        self.rate_limit = rate_limit;
        self.sbom = sbom;
        self.revocation_fail_mode = revocation_fail_mode;
        self
    }

    pub fn with_webhooks(mut self, webhooks: crate::webhooks::Dispatcher) -> Self {
        self.webhooks = webhooks;
        self
    }

    pub fn with_scheduler(mut self, scheduler: &crate::config::SchedulerConfig) -> Self {
        self.overcommit = crate::scheduler::Overcommit {
            cpu: scheduler.cpu_overcommit,
            memory: scheduler.memory_overcommit,
        };
        self
    }

    pub fn with_failover(mut self, failover: crate::config::FailoverConfig) -> Self {
        self.failover = failover;
        self
    }

    fn sub_ca_snapshot(&self) -> Result<SubCaState, Status> {
        Ok(self
            .sub_ca
            .lock()
            .map_err(|_| Status::internal("sub-CA lock poisoned"))?
            .clone())
    }

    /// Add a freshly signed chain to the certificate inventory.
    ///
    /// Best-effort on purpose: the certificate has already been issued and
    /// returning an error here would make the caller believe issuance failed.
    /// A missing inventory row degrades observability, not correctness — the
    /// node still gets a working certificate and the next rotation re-records
    /// it.
    fn record_issued_cert(&self, chain_pem: &str, node_id: &str) {
        if let Err(error) = crate::pki::inventory::record_signed_chain(&self.db, chain_pem, node_id)
        {
            warn!(%error, node_id = %node_id, "failed to record issued certificate in inventory");
        }
    }

    /// Revoke every still-valid certificate issued to `node_id`.
    ///
    /// Reason code 5 is RFC 5280 `cessationOfOperation`. The revocation cache
    /// and CRL are updated the same way as `RevokeCertificate`.
    fn revoke_node_certificates(&self, node_id: &str) -> Result<i32, Status> {
        let serials = self
            .db
            .find_revocable_serials("", node_id)
            .map_err(internal_db)?;
        let revoked_at = crate::pki::format_ts(time::OffsetDateTime::now_utc());
        let mut revoked = 0i32;
        for serial in &serials {
            let row = self
                .db
                .revoke_certificate_by_serial(serial, 5, &revoked_at)
                .map_err(internal_db)?;
            if let Some(row) = row {
                self.pki.revocation.insert_revoked(&row.serial_hex);
                revoked += 1;
            }
        }
        let sub_ca = self.sub_ca_snapshot()?;
        if let Err(error) = crate::pki::crl::ensure_current(
            &self.db,
            &sub_ca,
            &self.pki.crl_cache,
            time::Duration::hours(self.pki.pki.crl_validity_hours),
            time::Duration::hours(self.pki.pki.crl_refresh_before_hours),
            true,
        ) {
            warn!(%error, node_id, "node certificates revoked but CRL regeneration failed");
        }
        Ok(revoked)
    }

    /// Serials a revoke request refers to.
    ///
    /// `serial_hex` wins when present; otherwise every non-revoked
    /// certificate belonging to the named subject or node is selected, so
    /// revoking an identity does not leave an older still-valid certificate
    /// for the same subject usable.
    fn resolve_revocation_targets(
        &self,
        req: &controller_proto::RevokeCertificateRequest,
    ) -> Result<Vec<String>, Status> {
        let serial = crate::pki::normalize_serial(&req.serial_hex);
        if !serial.is_empty() {
            return match self.resolve_serial(&serial)? {
                Some(found) => Ok(vec![found]),
                None => Ok(Vec::new()),
            };
        }
        let subject_cn = req.subject_cn.trim();
        let node_id = req.node_id.trim();
        if subject_cn.is_empty() && node_id.is_empty() {
            return Err(Status::invalid_argument(
                "one of serial_hex, subject_cn or node_id is required",
            ));
        }
        self.db
            .find_revocable_serials(subject_cn, node_id)
            .map_err(internal_db)
    }

    /// Match an operator-supplied serial against the inventory, tolerating the
    /// leading-zero difference between DER integer bytes and the way some
    /// tools print serials.
    fn resolve_serial(&self, serial: &str) -> Result<Option<String>, Status> {
        let mut candidates = vec![serial.to_string()];
        let stripped = serial.trim_start_matches('0');
        if !stripped.is_empty() && stripped != serial {
            candidates.push(stripped.to_string());
        }
        if serial.len() % 2 == 1 {
            candidates.push(format!("0{serial}"));
        } else {
            candidates.push(format!("00{serial}"));
        }
        for candidate in candidates {
            if self
                .db
                .get_issued_certificate(&candidate)
                .map_err(internal_db)?
                .is_some()
            {
                return Ok(Some(candidate));
            }
        }
        Ok(None)
    }

    #[cfg(test)]
    pub fn new_with_test_push_hook(
        db: Database,
        clients: NodeClients,
        default_network: NetworkConfig,
        replication: Option<ReplicationConfig>,
        bootstrap_kctl: bool,
        hook: PushHook,
    ) -> Self {
        Self {
            db,
            clients,
            default_network,
            sub_ca: Arc::new(Mutex::new(SubCaState::default())),
            replication,
            tls_paths: None,
            require_manual_approval: false,
            bootstrap_kctl,
            pki: PkiRuntime::default(),
            rate_limit: crate::config::RateLimitConfig::default(),
            sbom: crate::config::SbomConfig::default(),
            revocation_fail_mode: crate::config::RevocationConfig::default().fail_mode,
            webhooks: crate::webhooks::Dispatcher::noop(),
            overcommit: crate::scheduler::Overcommit::default(),
            failover: crate::config::FailoverConfig::default(),
            web_consoles: WebConsoles::default(),
            test_push_hook: Some(hook),
            test_fence_hook: None,
            test_storage_verdict: None,
        }
    }

    #[cfg(test)]
    pub fn with_test_fence_hook(mut self, hook: FenceHook) -> Self {
        self.test_fence_hook = Some(hook);
        self
    }

    #[cfg(test)]
    pub fn with_test_storage_verdict(mut self, verdict: crate::ha::StorageVerdict) -> Self {
        self.test_storage_verdict = Some(verdict);
        self
    }

    #[inline]
    fn require_operator<T>(&self, request: &Request<T>, role: OperatorRole) -> Result<(), Status> {
        auth::require_controller_operator(
            request,
            &self.db,
            self.tls_paths.is_some(),
            self.bootstrap_kctl,
            role,
        )
    }

    /// Generate and push this node's Nix configuration, returning as soon as
    /// the node has accepted it. The `nixos-rebuild` it triggers is still
    /// running; use [`Self::push_config_and_await_apply`] when the next step
    /// depends on the new configuration being live.
    async fn push_config_to_node(&self, node: &NodeRow) -> Result<(), Status> {
        self.push_config_to_node_inner(node, false, None).await
    }

    /// Push this node's Nix configuration and block until the node reports the
    /// rebuild activated.
    ///
    /// `ApplyNixConfig` only *starts* `nixos-rebuild`, so anything that assumes
    /// the generated systemd unit already exists — starting a migrated VM on
    /// its destination, booting a freshly created one, believing a drained node
    /// has really let go of its VMs — has to wait for the verdict first.
    ///
    /// A node agent that predates apply tracking answers with an empty
    /// `apply_id`. There is nothing to poll then, so the caller carries on with
    /// the old fire-and-forget behaviour rather than failing the operation.
    async fn push_config_and_await_apply(&self, node: &NodeRow) -> Result<(), Status> {
        self.push_config_to_node_inner(node, true, None).await
    }

    async fn push_config_to_node_inner(
        &self,
        node: &NodeRow,
        await_apply: bool,
        incoming: Option<&VmRow>,
    ) -> Result<(), Status> {
        #[cfg(test)]
        if let Some(hook) = &self.test_push_hook {
            let _ = await_apply;
            return hook(node);
        }

        let vms = self
            .db
            .list_vms_for_node(&node.id)
            .map_err(|e| Status::internal(format!("listing vms: {e}")))?;
        let mut vms = vms;
        let mut incoming_ids = std::collections::HashSet::new();
        if let Some(vm) = incoming {
            incoming_ids.insert(vm.id.clone());
            if !vms.iter().any(|existing| existing.id == vm.id) {
                vms.push(vm.clone());
            }
        }
        let networks = self
            .db
            .list_networks_for_node(&node.id)
            .map_err(|e| Status::internal(format!("listing networks: {e}")))?;
        if let Some(vm) = incoming {
            if vm.network != "default" && !networks.iter().any(|net| net.name == vm.network) {
                return Err(Status::failed_precondition(format!(
                    "destination node {} has no network '{}' for VM '{}'",
                    node.id, vm.network, vm.name
                )));
            }
        }

        let iface = if node.gateway_interface.is_empty() {
            &self.default_network.gateway_interface
        } else {
            &node.gateway_interface
        };

        let mut vm_ssh_keys: std::collections::HashMap<String, Vec<String>> =
            std::collections::HashMap::new();
        for vm in &vms {
            match self.db.get_vm_ssh_keys(&vm.id) {
                Ok(keys) if !keys.is_empty() => {
                    vm_ssh_keys.insert(vm.id.clone(), keys);
                }
                _ => {}
            }
        }

        let node_ip = node.address.split(':').next().unwrap_or("").to_string();
        let all_nodes = self
            .db
            .list_nodes()
            .map_err(|e| Status::internal(format!("listing nodes for vxlan peers: {e}")))?;
        let eligible_peers = crate::net_policy::vxlan_peer_nodes(&all_nodes, &node.id);

        let mut vxlan_peers: std::collections::HashMap<String, nixgen::VxlanMeta> =
            std::collections::HashMap::new();
        for net in &networks {
            if net.network_type == "vxlan" {
                let all_with_name = self
                    .db
                    .list_networks_by_name(&net.name)
                    .map_err(|e| Status::internal(format!("listing vxlan peers: {e}")))?;
                let member_ids: std::collections::HashSet<&str> =
                    all_with_name.iter().map(|n| n.node_id.as_str()).collect();
                let mut seen_ips = std::collections::HashSet::new();
                let peers: Vec<String> = eligible_peers
                    .iter()
                    .filter(|peer| member_ids.contains(peer.id.as_str()))
                    .filter_map(|peer| {
                        let ip = peer.address.split(':').next().unwrap_or("").to_string();
                        if ip.is_empty() || !seen_ips.insert(ip.clone()) {
                            None
                        } else {
                            Some(ip)
                        }
                    })
                    .collect();
                vxlan_peers.insert(
                    net.name.clone(),
                    nixgen::VxlanMeta {
                        vni: net.vni,
                        peers,
                        local_ip: node_ip.clone(),
                    },
                );
            }
        }

        let mut security_group_rules: HashMap<String, Vec<nixgen::SecurityGroupResolvedRule>> =
            HashMap::new();
        for net in &networks {
            let mut effective_groups = self
                .db
                .list_security_groups_for_network(&net.name, &node.id)
                .map_err(|e| Status::internal(format!("listing network security groups: {e}")))?;
            for vm in vms.iter().filter(|v| v.network == net.name) {
                let vm_groups = self
                    .db
                    .list_security_groups_for_vm(&vm.id)
                    .map_err(|e| Status::internal(format!("listing vm security groups: {e}")))?;
                effective_groups.extend(vm_groups);
            }
            effective_groups.sort();
            effective_groups.dedup();
            let mut rules_for_net = Vec::new();
            for sg_name in effective_groups {
                let sg_rules = self
                    .db
                    .list_security_group_rules(&sg_name)
                    .map_err(|e| Status::internal(format!("listing security group rules: {e}")))?;
                for rule in sg_rules {
                    let target_ip = if !rule.target_vm.trim().is_empty() {
                        if let Some(target_vm) = self
                            .db
                            .get_vm(rule.target_vm.trim())
                            .map_err(|e| Status::internal(e.to_string()))?
                            .or_else(|| {
                                self.db.list_vms_for_node(&node.id).ok().and_then(|vv| {
                                    vv.into_iter().find(|v| v.name == rule.target_vm)
                                })
                            })
                        {
                            target_vm.vm_ip
                        } else {
                            String::new()
                        }
                    } else {
                        String::new()
                    };
                    rules_for_net.push(nixgen::SecurityGroupResolvedRule {
                        protocol: rule.protocol,
                        host_port: rule.host_port,
                        target_port: if rule.target_port <= 0 {
                            rule.host_port
                        } else {
                            rule.target_port
                        },
                        source_cidr: if rule.source_cidr.trim().is_empty() {
                            "0.0.0.0/0".to_string()
                        } else {
                            rule.source_cidr
                        },
                        target_ip,
                        enable_dnat: rule.enable_dnat,
                    });
                }
            }
            if !rules_for_net.is_empty() {
                security_group_rules.insert(net.name.clone(), rules_for_net);
            }
        }

        let postgresql = self
            .db
            .get_postgresql_for_node(&node.id)
            .map_err(|e| Status::internal(format!("listing postgresql: {e}")))?;
        let postgresql_nix = postgresql.as_ref().map(|row| nixgen::PostgresqlNix {
            database: row.database_name.clone(),
            package: row.package.clone(),
            port: row.port,
        });
        let guest_ops_keys = crate::guest_ops::all_guest_ops_keys(&self.db);
        let mut admin = self.ensure_admin_client_for_node(node).await?;
        let module_features = self.ch_vm_module_features(node, &mut admin).await;
        let vgpu = self.vgpu_slices_for(&node.id, &vms)?;
        if !vgpu.is_empty() && !module_features.vgpu_profile {
            return Err(Status::failed_precondition(format!(
                "node {} cannot start a vGPU slice until its ch-vm module reports vgpuProfile",
                node.id
            )));
        }
        let ingress_nix = crate::ingress_reconciler::load_state(&self.db)
            .ok()
            .and_then(|st| crate::ingress_reconciler::ingress_nix_for_node(&st, node));
        let nix_config = nixgen::with_postgresql(
            nixgen::generate_node_config_inner(
                &vms,
                iface,
                &self.default_network,
                &networks,
                &vm_ssh_keys,
                &vxlan_peers,
                &security_group_rules,
                &incoming_ids,
                &self
                    .db
                    .list_volumes()
                    .map_err(|e| Status::internal(format!("listing volumes for nixgen: {e}")))?,
                &guest_ops_keys,
                &extra_nics_by_vm(&self.db, &vms),
                &network_policies_by_name(&self.db, &node.id)?,
                &vm_ipv6_by_vm(&self.db, &vms)?,
                &module_features,
                &vgpu,
            ),
            postgresql_nix.as_ref(),
        );
        let nix_config = nixgen::with_ingress(nix_config, ingress_nix.as_ref());

        for vm in &vms {
            if vm.image_url.is_empty() {
                continue;
            }
            let ensure = admin
                .ensure_image(node_proto::EnsureImageRequest {
                    image_url: vm.image_url.clone(),
                    image_sha256: vm.image_sha256.clone(),
                    destination_path: vm.image_path.clone(),
                })
                .await
                .map_err(|e| {
                    error!(node = %node.id, vm_id = %vm.id, error = %e, "failed to ensure vm image on node");
                    Status::internal(format!("ensuring image for vm {} on node {}: {e}", vm.id, node.id))
                })?
                .into_inner();
            info!(
                node = %node.id,
                vm_id = %vm.id,
                path = %ensure.path,
                size_bytes = ensure.size_bytes,
                cached = ensure.cached,
                downloaded = ensure.downloaded,
                "ensured vm image on node"
            );
        }

        // E2: unlock host LUKS for encrypted volumes before the VM unit starts.
        if let Ok(vols) = self.db.list_volumes() {
            let on_node: Vec<_> = vols
                .into_iter()
                .filter(|v| v.encrypted && v.is_attached() && vms.iter().any(|vm| vm.id == v.vm_id))
                .collect();
            if !on_node.is_empty() {
                if self.clients.get_storage(&node.address).is_none() {
                    let _ = self.clients.connect(&node.address).await;
                }
                if let Some(mut storage) = self.clients.get_storage(&node.address) {
                    if let Ok(keys) = self.encrypted_volume_keys_for_vols(&on_node) {
                        for key in keys {
                            let _ = storage
                                .unlock_encrypted_volume(node_proto::UnlockEncryptedVolumeRequest {
                                    rbd_device: format!("{}/{}", key.pool, key.image),
                                    dek: key.dek,
                                    mapper_name: key.mapper_name,
                                    resize: false,
                                })
                                .await;
                        }
                    }
                }
            }
        }

        let apply_id = Uuid::new_v4().to_string();
        let apply = admin
            .apply_nix_config(node_proto::ApplyNixConfigRequest {
                configuration_nix: nix_config,
                rebuild: true,
                apply_id: apply_id.clone(),
            })
            .await
            .map_err(|e| {
                error!(node = %node.id, error = %e, "failed to push config to node");
                Status::internal(format!("pushing config to node {}: {e}", node.id))
            })?
            .into_inner();
        if !apply.success {
            error!(
                node = %node.id,
                message = %apply.message,
                "node rejected nix config apply request"
            );
            return Err(Status::internal(format!(
                "node {} rejected nix apply: {}",
                node.id, apply.message
            )));
        }

        info!(
            node = %node.id,
            message = %apply.message,
            "node accepted nix config apply request"
        );

        info!(node = %node.id, "pushed config and triggered rebuild");

        if await_apply {
            self.await_nix_apply(node, &apply.apply_id, &mut admin)
                .await?;
        }
        Ok(())
    }

    /// Poll `GetNixApplyStatus` until the node's rebuild reaches a verdict.
    ///
    /// `nixos-rebuild switch` restarts the node agent, so the node records the
    /// verdict from a transient unit that outlives the agent and a dropped
    /// connection mid-rebuild is normal — transport errors are retried until
    /// the deadline rather than reported.
    async fn await_nix_apply(
        &self,
        node: &NodeRow,
        apply_id: &str,
        admin: &mut node_proto::node_admin_client::NodeAdminClient<tonic::transport::Channel>,
    ) -> Result<(), Status> {
        if apply_id.is_empty() {
            warn!(
                node = %node.id,
                "node agent returned no apply_id; cannot wait for the rebuild to activate"
            );
            return Ok(());
        }
        let deadline = tokio::time::Instant::now() + NIX_APPLY_WAIT_TIMEOUT;
        let mut last_message;
        loop {
            match admin
                .get_nix_apply_status(node_proto::GetNixApplyStatusRequest {
                    apply_id: apply_id.to_string(),
                })
                .await
            {
                Ok(resp) => {
                    let resp = resp.into_inner();
                    last_message = resp.message;
                    match nix_apply_progress(resp.phase) {
                        NixApplyProgress::Activated => {
                            info!(node = %node.id, %apply_id, "node activated the pushed configuration");
                            return Ok(());
                        }
                        NixApplyProgress::Failed => {
                            return Err(Status::internal(format!(
                                "nixos-rebuild for apply {apply_id} failed on node {}: {last_message}",
                                node.id
                            )));
                        }
                        // Degrade to the old unsynchronised behaviour: there is
                        // no answer coming, and failing the operation over a
                        // missing verdict would be worse than proceeding.
                        NixApplyProgress::NoVerdict => {
                            warn!(
                                node = %node.id,
                                %apply_id,
                                message = %last_message,
                                "node has no verdict for this nix apply; continuing without the barrier"
                            );
                            return Ok(());
                        }
                        NixApplyProgress::Pending => {}
                    }
                }
                // An agent without apply tracking cannot be waited on.
                Err(e) if e.code() == tonic::Code::Unimplemented => {
                    warn!(
                        node = %node.id,
                        %apply_id,
                        "node agent does not implement GetNixApplyStatus; continuing without the barrier"
                    );
                    return Ok(());
                }
                Err(e) if e.code() == tonic::Code::Unavailable => {
                    debug!(
                        node = %node.id,
                        %apply_id,
                        error = %e,
                        "node unreachable while its rebuild runs; retrying"
                    );
                    last_message = e.message().to_string();
                }
                Err(e) => {
                    return Err(Status::internal(format!(
                        "polling nix apply {apply_id} on node {}: {e}",
                        node.id
                    )));
                }
            }
            if tokio::time::Instant::now() >= deadline {
                return Err(Status::deadline_exceeded(format!(
                    "node {} did not finish activating apply {apply_id} within {}s: {last_message}",
                    node.id,
                    NIX_APPLY_WAIT_TIMEOUT.as_secs()
                )));
            }
            tokio::time::sleep(NIX_APPLY_POLL_INTERVAL).await;
        }
    }

    /// Re-push config to all other nodes sharing a VXLAN network so their
    /// FDB peer lists stay current when a node joins or leaves the overlay.
    async fn refresh_vxlan_peers(&self, network_name: &str, skip_node_id: &str) {
        let peer_networks = match self.db.list_networks_by_name(network_name) {
            Ok(nets) => nets,
            Err(e) => {
                warn!(network = %network_name, error = %e, "failed to list vxlan peer networks");
                return;
            }
        };
        for peer_net in &peer_networks {
            if peer_net.node_id == skip_node_id {
                continue;
            }
            let peer_node = match self.db.get_node(&peer_net.node_id) {
                Ok(Some(n)) => n,
                _ => continue,
            };
            if let Err(e) = self.push_config_to_node(&peer_node).await {
                warn!(
                    node = %peer_node.id,
                    network = %network_name,
                    error = %e,
                    "failed to refresh vxlan peers on node"
                );
            } else {
                info!(
                    node = %peer_node.id,
                    network = %network_name,
                    "refreshed vxlan peer list"
                );
            }
        }
    }

    #[allow(clippy::result_large_err)]
    fn lookup_node(&self, target: &str) -> Result<NodeRow, Status> {
        self.db
            .get_node_by_address(target)
            .map_err(|e| Status::internal(e.to_string()))?
            .or_else(|| self.db.get_node(target).ok().flatten())
            .ok_or_else(|| Status::not_found(format!("node {target} not found")))
    }

    #[allow(clippy::result_large_err)]
    fn resolve_node_for_vm(&self, vm_id: &str, target_node: &str) -> Result<NodeRow, Status> {
        if !target_node.is_empty() {
            let node = self
                .db
                .get_node_by_address(target_node)
                .map_err(|e| Status::internal(e.to_string()))?
                .or_else(|| self.db.get_node(target_node).ok().flatten())
                .ok_or_else(|| Status::not_found(format!("node {target_node} not found")))?;
            return Ok(node);
        }

        let node_id = self
            .db
            .find_node_for_vm(vm_id)
            .map_err(|e| Status::internal(e.to_string()))?
            .ok_or_else(|| Status::not_found(format!("VM {vm_id} not found")))?;

        self.db
            .get_node(&node_id)
            .map_err(|e| Status::internal(e.to_string()))?
            .ok_or_else(|| Status::not_found(format!("node {node_id} not found")))
    }

    fn ensure_explicit_placement(
        &self,
        node: &NodeRow,
        required_labels: &[String],
        anti_affinity: &str,
    ) -> Result<(), Status> {
        if !required_labels.is_empty() {
            let have = self
                .db
                .get_node_labels(&node.id)
                .map_err(|e| Status::internal(format!("listing node labels: {e}")))?;
            let missing: Vec<&str> = required_labels
                .iter()
                .filter(|want| !have.iter().any(|got| got == *want))
                .map(String::as_str)
                .collect();
            if !missing.is_empty() {
                return Err(Status::failed_precondition(format!(
                    "node '{}' is missing label(s) {}",
                    node.id,
                    missing.join(", ")
                )));
            }
        }
        if !anti_affinity.is_empty() {
            let groups = self
                .db
                .anti_affinity_groups_by_node()
                .map_err(|e| Status::internal(format!("listing anti-affinity groups: {e}")))?;
            if groups
                .get(&node.id)
                .map(|existing| existing.iter().any(|group| group == anti_affinity))
                .unwrap_or(false)
            {
                return Err(Status::failed_precondition(format!(
                    "node '{}' already hosts anti-affinity group '{anti_affinity}'",
                    node.id
                )));
            }
        }
        Ok(())
    }

    fn select_scheduled_node(
        &self,
        nodes: &[NodeRow],
        cpu: i32,
        memory: i64,
        dc: &str,
        required_labels: &[String],
        anti_affinity: &str,
    ) -> Result<Option<NodeRow>, Status> {
        let label_rows = self
            .db
            .get_all_node_labels()
            .map_err(|e| Status::internal(format!("listing node labels: {e}")))?;
        let mut node_labels: std::collections::HashMap<String, Vec<String>> =
            std::collections::HashMap::new();
        for (node_id, label) in label_rows {
            node_labels.entry(node_id).or_default().push(label);
        }
        let groups = self
            .db
            .anti_affinity_groups_by_node()
            .map_err(|e| Status::internal(format!("listing anti-affinity groups: {e}")))?;
        let constraints = crate::scheduler::Constraints {
            overcommit: self.overcommit,
            dc,
            required_labels,
            node_labels: &node_labels,
            anti_affinity,
            groups_on_node: &groups,
        };
        Ok(crate::scheduler::select_with_constraints(nodes, cpu, memory, &constraints).cloned())
    }

    fn preflight_vm_create_on_node(
        &self,
        node: &NodeRow,
        spec: &controller_proto::VmSpec,
        requested_storage_backend: &str,
    ) -> Result<(), Status> {
        let alternative_nodes = self.alternative_vm_create_nodes(
            &node.id,
            requested_storage_backend,
            spec.cpu,
            spec.memory_bytes,
        );
        let alternative_ids = alternative_nodes
            .into_iter()
            .map(|n| n.id)
            .take(3)
            .collect::<Vec<_>>();
        let hint = if alternative_ids.is_empty() {
            String::new()
        } else {
            format!("; try target_node one of: {}", alternative_ids.join(", "))
        };

        if node.approval_status != "approved" {
            return Err(Status::failed_precondition(format!(
                "node '{}' is not approved{}",
                node.id, hint
            )));
        }
        if node.status != "ready" {
            return Err(Status::unavailable(format!(
                "node '{}' is not ready{}",
                node.id, hint
            )));
        }

        let available_cpu = crate::scheduler::schedulable_cpu(node.cpu_cores, self.overcommit.cpu)
            - i64::from(node.cpu_used);
        let available_memory =
            crate::scheduler::schedulable_memory(node.memory_bytes, self.overcommit.memory)
                - node.memory_used;
        if available_cpu < i64::from(spec.cpu) || available_memory < spec.memory_bytes {
            return Err(Status::unavailable(format!(
                "node '{}' lacks capacity for request (need cpu={} mem={}, available cpu={} mem={}){}",
                node.id, spec.cpu, spec.memory_bytes, available_cpu, available_memory, hint
            )));
        }
        Ok(())
    }

    fn alternative_vm_create_nodes(
        &self,
        exclude_node_id: &str,
        requested_storage_backend: &str,
        cpu: i32,
        memory_bytes: i64,
    ) -> Vec<NodeRow> {
        self.db
            .list_nodes()
            .ok()
            .unwrap_or_default()
            .into_iter()
            .filter(|n| {
                n.id != exclude_node_id
                    && self.node_supports_backend(n, requested_storage_backend)
                    && n.approval_status == "approved"
                    && n.status == "ready"
                    && crate::scheduler::node_has_capacity(n, cpu, memory_bytes, self.overcommit)
            })
            .collect()
    }

    pub(crate) fn ceph_member_ids(&self) -> HashSet<String> {
        let clusters = match self.db.list_ceph_clusters() {
            Ok(clusters) => clusters,
            Err(e) => {
                warn!(
                    error = %e,
                    "could not list Ceph clusters; no host will be treated as a Ceph member"
                );
                return HashSet::new();
            }
        };
        clusters
            .into_iter()
            .filter_map(|c| ceph_cluster_spec::spec_from_json(&c.spec_json).ok())
            .flat_map(|s| s.nodes.into_iter().map(|n| n.node_id))
            .collect()
    }

    pub(crate) fn node_supports_backend(&self, node: &NodeRow, backend: &str) -> bool {
        node.storage_backend == backend
            || (backend == "ceph" && self.ceph_member_ids().contains(&node.id))
    }

    /// Names of Ceph-backed VMs that still depend on the given CephCluster,
    /// either because they sit on one of its member nodes or because they own a
    /// `volumes` row in a pool the cluster serves.
    pub(crate) fn ceph_cluster_vms_in_use(
        &self,
        cluster_name: &str,
    ) -> Result<Vec<String>, Status> {
        let Some(cluster) = self
            .db
            .get_ceph_cluster(cluster_name)
            .map_err(|e| Status::internal(e.to_string()))?
        else {
            return Ok(Vec::new());
        };
        let spec = ceph_cluster_spec::spec_from_json(&cluster.spec_json)
            .map_err(|e| Status::internal(format!("decode ceph cluster spec: {e}")))?;
        let members: HashSet<String> = spec.nodes.into_iter().map(|n| n.node_id).collect();
        let mut names: Vec<String> = self
            .db
            .list_vms()
            .map_err(|e| Status::internal(e.to_string()))?
            .into_iter()
            .filter(|vm| vm.storage_backend == "ceph" && members.contains(&vm.node_id))
            .map(|vm| vm.name)
            .collect();
        names.sort();
        names.dedup();
        Ok(names)
    }

    /// True when `node_id` belongs to a CephCluster whose reconciled status is
    /// `healthy`. Placing a shared-RBD VM on a node outside a healthy cluster
    /// gives a guest that cannot map its own disk.
    pub(crate) fn is_healthy_ceph_member(&self, node_id: &str) -> Result<bool, Status> {
        let clusters = self
            .db
            .list_ceph_clusters()
            .map_err(|e| Status::internal(e.to_string()))?;
        for cluster in clusters {
            let Ok(Some(status)) = self.db.get_ceph_cluster_status(&cluster.name) else {
                continue;
            };
            if status.phase != "healthy" {
                continue;
            }
            let member = ceph_cluster_spec::spec_from_json(&cluster.spec_json)
                .map(|s| s.nodes.iter().any(|n| n.node_id == node_id))
                .unwrap_or(false);
            if member {
                return Ok(true);
            }
        }
        Ok(false)
    }

    fn resolve_vm_row(&self, key: &str) -> Result<Option<VmRow>, Status> {
        match self.resolve_vm(key) {
            Ok(vm) => Ok(Some(vm)),
            Err(e) if e.code() == tonic::Code::NotFound => Ok(None),
            Err(e) => Err(e),
        }
    }

    fn resolve_volume_row(&self, key: &str) -> Result<Option<VolumeRow>, Status> {
        let key = key.trim();
        if key.is_empty() {
            return Err(Status::invalid_argument("volume name is required"));
        }
        if let Some(v) = self
            .db
            .get_volume_by_name(key)
            .map_err(|e| Status::internal(e.to_string()))?
        {
            return Ok(Some(v));
        }
        self.db
            .get_volume_by_id(key)
            .map_err(|e| Status::internal(e.to_string()))
    }

    fn volume_info_from_row(
        &self,
        vol: &VolumeRow,
        vm: Option<&VmRow>,
    ) -> controller_proto::VolumeInfo {
        controller_proto::VolumeInfo {
            id: vol.id.clone(),
            name: vol.name.clone(),
            role: vol.role.clone(),
            attach_state: vol.attach_state.clone(),
            serial: vol.serial.clone(),
            pool: vol.pool.clone(),
            image: vol.image.clone(),
            slot: vol.slot,
            vm_id: vol.vm_id.clone(),
            vm_name: vm.map(|v| v.name.clone()).unwrap_or_default(),
            node_id: if !vol.host_node_id.is_empty() {
                vol.host_node_id.clone()
            } else {
                vm.map(|v| v.node_id.clone()).unwrap_or_default()
            },
            storage_backend: if vol.storage_class.is_empty() {
                "ceph".into()
            } else {
                vol.storage_class.clone()
            },
            storage_size_bytes: vol.size_bytes,
            backend_handle: if vol.encrypted {
                format!(
                    "/dev/mapper/{}",
                    crate::volume_crypto::mapper_name_for_serial(&vol.serial)
                )
            } else if crate::host_volume::is_host(&vol.storage_class) {
                crate::host_volume::device_path(&vol.storage_class, &vol.pool, &vol.image)
                    .unwrap_or_default()
            } else {
                format!("/dev/rbd/{}/{}", vol.pool, vol.image)
            },
            image_format: "raw".into(),
            vm_state: vm
                .map(|v| state_fallback_without_runtime(v.auto_start))
                .unwrap_or(0),
            guest_visible_bytes: vol.guest_visible_bytes,
            guest_checked_at: vol.guest_checked_at.clone(),
            encrypted: vol.encrypted,
            mount_point: vol.mount_point.clone(),
            fs_type: vol.fs_type.clone(),
        }
    }

    async fn require_vm_stopped_for_volume_change(&self, vm: &VmRow) -> Result<(), Status> {
        if self.vm_is_live(vm).await? {
            return Err(Status::failed_precondition(format!(
                "VM '{}' must be stopped for this volume operation",
                vm.name
            )));
        }
        Ok(())
    }

    /// True when the guest VMM reports Running/Paused (hotplug path).
    async fn vm_is_live(&self, vm: &VmRow) -> Result<bool, Status> {
        if !vm.auto_start {
            return Ok(false);
        }
        let node = self
            .db
            .get_node(&vm.node_id)
            .map_err(|e| Status::internal(e.to_string()))?
            .ok_or_else(|| Status::failed_precondition("VM node missing"))?;
        if self.clients.get_compute(&node.address).is_none() {
            let _ = self.clients.connect(&node.address).await;
        }
        if let Some(mut compute) = self.clients.get_compute(&node.address) {
            if let Ok(Ok(resp)) = tokio::time::timeout(
                Duration::from_secs(3),
                compute.get_vm(node_proto::GetVmRequest {
                    vm_id: vm.name.clone(),
                }),
            )
            .await
            {
                if let Some(status) = resp.into_inner().status {
                    let state = controller_state_from_node_state(status.state);
                    return Ok(state == controller_proto::VmState::Running as i32
                        || state == controller_proto::VmState::Paused as i32);
                }
            }
        }
        Ok(false)
    }

    async fn guest_reachable_for(&self, vm: &VmRow) -> bool {
        let running = vm.runtime_state == "running" || vm.auto_start;
        if vm.vm_ip.is_empty() || !running {
            return false;
        }
        let Ok(Some(node)) = self.db.get_node(&vm.node_id) else {
            return false;
        };
        let Some(mut admin) = self.clients.get_admin(&node.address) else {
            return true;
        };
        let req = node_proto::GuestDiskProbeRequest {
            vm_name: sanitize_nix_attr_key(&vm.name),
            network: vm.network.clone(),
            disk_serial: String::new(),
            ssh_user: String::new(),
            port: 22,
            timeout_ms: 1500,
        };
        match tokio::time::timeout(Duration::from_secs(2), admin.guest_disk_probe(req)).await {
            Ok(Ok(resp)) => resp.into_inner().success,
            _ => false,
        }
    }

    async fn guest_fs_freeze_for_vm(
        &self,
        vm: &VmRow,
        freeze: bool,
        pre_script: &str,
        post_script: &str,
    ) -> Result<(), Status> {
        let node = self
            .db
            .get_node(&vm.node_id)
            .map_err(|e| Status::internal(e.to_string()))?
            .ok_or_else(|| Status::failed_precondition("VM node missing"))?;
        if self.clients.get_admin(&node.address).is_none() {
            let _ = self.clients.connect(&node.address).await;
        }
        let mut admin = self.clients.get_admin(&node.address).ok_or_else(|| {
            Status::failed_precondition(
                "quiesced snapshot requires GuestOps SSH to the guest; node admin client unavailable",
            )
        })?;
        let runtime_name = sanitize_nix_attr_key(&vm.name);
        admin
            .guest_fs_freeze(node_proto::GuestFsFreezeRequest {
                vm_name: runtime_name,
                network: vm.network.clone(),
                ssh_user: String::new(),
                port: 22,
                timeout_ms: 60_000,
                freeze,
                pre_script: pre_script.to_string(),
                post_script: post_script.to_string(),
            })
            .await
            .map_err(|e| {
                Status::failed_precondition(format!(
                    "quiesced snapshot requires GuestOps SSH to the guest: {e}"
                ))
            })?;
        Ok(())
    }

    fn node_for_instant(&self, name: &str) -> Result<crate::db::NodeRow, Status> {
        let nodes = self
            .db
            .list_nodes()
            .map_err(|e| Status::internal(e.to_string()))?;
        if name.is_empty() {
            return nodes
                .into_iter()
                .next()
                .ok_or_else(|| Status::failed_precondition("no nodes"));
        }
        nodes
            .into_iter()
            .find(|n| n.id == name || n.hostname == name)
            .ok_or_else(|| Status::not_found(format!("node {name} not found")))
    }

    /// Close the LUKS mapper (and release its nbd) on `node` for each encrypted
    /// volume. Best effort: an unreachable node must not block detach/delete.
    async fn lock_encrypted_volumes_on_node(&self, node: &NodeRow, vols: &[VolumeRow]) {
        let encrypted: Vec<&VolumeRow> = vols.iter().filter(|v| v.encrypted).collect();
        if encrypted.is_empty() {
            return;
        }
        if self.clients.get_storage(&node.address).is_none() {
            let _ = self.clients.connect(&node.address).await;
        }
        let Some(mut storage) = self.clients.get_storage(&node.address) else {
            return;
        };
        for vol in encrypted {
            let mapper = crate::volume_crypto::mapper_name_for_serial(&vol.serial);
            if let Err(e) = storage
                .lock_encrypted_volume(node_proto::LockEncryptedVolumeRequest {
                    mapper_name: mapper,
                })
                .await
            {
                warn!(volume = %vol.name, node = %node.id, error = %e, "failed to lock encrypted volume");
            }
        }
    }

    /// After `rbd resize`, grow the LUKS mapper on the node that has it open.
    async fn grow_encrypted_mapper(&self, vol: &VolumeRow) -> Result<(), Status> {
        let vm = self
            .db
            .get_vm(&vol.vm_id)
            .map_err(|e| Status::internal(e.to_string()))?
            .ok_or_else(|| Status::failed_precondition("attached VM missing"))?;
        let node = self
            .db
            .get_node(&vm.node_id)
            .map_err(|e| Status::internal(e.to_string()))?
            .ok_or_else(|| Status::failed_precondition("VM node missing"))?;
        if self.clients.get_storage(&node.address).is_none() {
            let _ = self.clients.connect(&node.address).await;
        }
        let mut storage = self
            .clients
            .get_storage(&node.address)
            .ok_or_else(|| Status::unavailable("storage client unavailable"))?;
        let keys = self.encrypted_volume_keys_for_vols(std::slice::from_ref(vol))?;
        for key in keys {
            storage
                .unlock_encrypted_volume(node_proto::UnlockEncryptedVolumeRequest {
                    rbd_device: format!("{}/{}", key.pool, key.image),
                    dek: key.dek,
                    mapper_name: key.mapper_name,
                    resize: true,
                })
                .await
                .map_err(|e| Status::internal(format!("grow LUKS mapper: {e}")))?;
        }
        Ok(())
    }

    async fn hotplug_add_volume_disk(&self, vm: &VmRow, vol: &VolumeRow) -> Result<(), Status> {
        let node = self
            .db
            .get_node(&vm.node_id)
            .map_err(|e| Status::internal(e.to_string()))?
            .ok_or_else(|| Status::failed_precondition("VM node missing"))?;
        if self.clients.get_storage(&node.address).is_none() {
            let _ = self.clients.connect(&node.address).await;
        }
        if self.clients.get_admin(&node.address).is_none() {
            let _ = self.clients.connect(&node.address).await;
        }
        // Map (+ unlock if encrypted) before CH add-disk.
        if let Some(mut storage) = self.clients.get_storage(&node.address) {
            if vol.encrypted {
                let keys = self.encrypted_volume_keys_for_vols(std::slice::from_ref(vol))?;
                for key in keys {
                    storage
                        .unlock_encrypted_volume(node_proto::UnlockEncryptedVolumeRequest {
                            rbd_device: format!("{}/{}", key.pool, key.image),
                            dek: key.dek,
                            mapper_name: key.mapper_name,
                            resize: false,
                        })
                        .await
                        .map_err(|e| Status::internal(format!("unlock for hotplug: {e}")))?;
                }
            } else {
                storage
                    .attach_volume(node_proto::AttachVolumeRequest {
                        backend_handle: format!("{}/{}", vol.pool, vol.image),
                        vm_id: vm.id.clone(),
                        target_device: vol.serial.clone(),
                        bus: "virtio".into(),
                    })
                    .await
                    .map_err(|e| Status::internal(format!("rbd map for hotplug: {e}")))?;
            }
        }
        let path = if vol.encrypted {
            format!(
                "/dev/mapper/{}",
                crate::volume_crypto::mapper_name_for_serial(&vol.serial)
            )
        } else {
            format!("/dev/rbd/{}/{}", vol.pool, vol.image)
        };
        let mut admin = self
            .clients
            .get_admin(&node.address)
            .ok_or_else(|| Status::unavailable("admin client unavailable for hotplug"))?;
        let runtime_name = sanitize_nix_attr_key(&vm.name);
        admin
            .hot_add_disk(node_proto::HotAddDiskRequest {
                vm_name: runtime_name,
                path,
                serial: vol.serial.clone(),
                readonly: false,
            })
            .await
            .map_err(|e| Status::internal(format!("hot-add disk: {e}")))?;
        Ok(())
    }

    async fn hotplug_remove_volume_disk(&self, vm: &VmRow, vol: &VolumeRow) -> Result<(), Status> {
        let node = self
            .db
            .get_node(&vm.node_id)
            .map_err(|e| Status::internal(e.to_string()))?
            .ok_or_else(|| Status::failed_precondition("VM node missing"))?;
        if self.clients.get_admin(&node.address).is_none() {
            let _ = self.clients.connect(&node.address).await;
        }
        let mut admin = self
            .clients
            .get_admin(&node.address)
            .ok_or_else(|| Status::unavailable("admin client unavailable for hotplug"))?;
        let runtime_name = sanitize_nix_attr_key(&vm.name);
        admin
            .hot_remove_disk(node_proto::HotRemoveDiskRequest {
                vm_name: runtime_name,
                disk_id: vol.serial.clone(),
            })
            .await
            .map_err(|e| Status::internal(format!("hot-remove disk: {e}")))?;
        Ok(())
    }

    async fn pick_healthy_ceph_node(&self) -> Result<NodeRow, String> {
        let nodes = self.db.list_nodes().map_err(|e| e.to_string())?;
        for node in nodes {
            if self
                .is_healthy_ceph_member(&node.id)
                .map_err(|e| e.to_string())?
            {
                return Ok(node);
            }
        }
        Err("no healthy CephCluster member is registered".into())
    }

    /// Node that should run snapshot/expose for this volume (host node for
    /// LVM/ZFS, any healthy Ceph member for RBD).
    async fn resolve_snapshot_node(&self, vol: &VolumeRow) -> Result<NodeRow, String> {
        let preferred = crate::volume_export::snapshot_node_id(vol)?;
        if preferred.is_empty() {
            return self.pick_healthy_ceph_node().await;
        }
        let node = self
            .db
            .get_node(&preferred)
            .map_err(|e| e.to_string())?
            .ok_or_else(|| format!("node '{preferred}' not found"))?;
        crate::volume_export::node_ready(&node)?;
        Ok(node)
    }

    fn vm_operation_to_proto(row: &VmOperationRow) -> controller_proto::VmOperation {
        controller_proto::VmOperation {
            id: row.id.clone(),
            vm_id: row.vm_id.clone(),
            kind: row.kind.clone(),
            phase: row.phase.clone(),
            source_node: row.source_node.clone(),
            target_node: row.target_node.clone(),
            cancel_requested: row.cancel_requested,
            send_succeeded: row.send_succeeded,
            detail_json: row.detail_json.clone(),
            started_at: parse_datetime_to_timestamp(&row.started_at),
            updated_at: parse_datetime_to_timestamp(&row.updated_at),
            finished_at: if row.finished_at.is_empty() {
                None
            } else {
                parse_datetime_to_timestamp(&row.finished_at)
            },
        }
    }

    /// Best-effort progress estimate for an open live migrate (CH has no byte
    /// counter). Queries the destination receive session.
    async fn live_migrate_progress_json(&self, op: &VmOperationRow) -> Result<String, String> {
        let vm = self
            .db
            .get_vm(&op.vm_id)
            .map_err(|e| e.to_string())?
            .ok_or_else(|| "VM missing".to_string())?;
        let target = self
            .db
            .get_node(&op.target_node)
            .map_err(|e| e.to_string())?
            .ok_or_else(|| "target missing".to_string())?;
        if self.clients.get_admin(&target.address).is_none() {
            let _ = self.clients.connect(&target.address).await;
        }
        let mut admin = self
            .clients
            .get_admin(&target.address)
            .ok_or_else(|| "admin unavailable".to_string())?;
        let runtime_name = sanitize_nix_attr_key(&vm.name);
        let prog = admin
            .get_live_migrate_progress(node_proto::GetLiveMigrateProgressRequest {
                vm_name: runtime_name,
            })
            .await
            .map_err(|e| e.to_string())?
            .into_inner();
        Ok(format!(
            r#"{{"progress":{{"estimate":{},"elapsedSeconds":{},"memoryBytes":{},"message":"{}"}}}}"#,
            prog.estimate,
            prog.elapsed_seconds,
            prog.memory_bytes,
            prog.message.replace('"', "'").replace('\n', " ")
        ))
    }

    fn volume_snapshot_to_proto(
        &self,
        row: &VolumeSnapshotRow,
        vol: Option<&VolumeRow>,
    ) -> controller_proto::VolumeSnapshotInfo {
        controller_proto::VolumeSnapshotInfo {
            id: row.id.clone(),
            name: row.name.clone(),
            volume_id: row.volume_id.clone(),
            volume_name: vol.map(|v| v.name.clone()).unwrap_or_default(),
            rbd_snap: row.rbd_snap.clone(),
            protected: row.protected,
            size_bytes: row.size_bytes,
            consistency: row.consistency.clone(),
            created_at: parse_datetime_to_timestamp(&row.created_at),
        }
    }

    fn snapshot_policy_to_proto(row: &SnapshotPolicyRow) -> controller_proto::SnapshotPolicy {
        controller_proto::SnapshotPolicy {
            name: row.name.clone(),
            selector_vm: row.selector_vm.clone(),
            selector_volume: row.selector_volume.clone(),
            schedule: row.schedule.clone(),
            keep: row.keep,
            enabled: row.enabled,
            last_run_at: row.last_run_at.clone(),
            last_message: row.last_message.clone(),
        }
    }

    fn encrypted_volume_keys_for_vols(
        &self,
        vols: &[VolumeRow],
    ) -> Result<Vec<node_proto::EncryptedVolumeKey>, Status> {
        let enc: Vec<&VolumeRow> = vols.iter().filter(|v| v.encrypted).collect();
        if enc.is_empty() {
            return Ok(vec![]);
        }
        let master = crate::volume_crypto::MasterKeyStore::default()
            .load_or_create()
            .map_err(|e| Status::failed_precondition(format!("volume master key: {e}")))?;
        let mut out = Vec::with_capacity(enc.len());
        for v in enc {
            let dek = crate::volume_crypto::unwrap_dek(&master, &v.wrapped_dek)
                .map_err(|e| Status::internal(format!("unwrap dek for {}: {e}", v.name)))?;
            out.push(node_proto::EncryptedVolumeKey {
                pool: v.pool.clone(),
                image: v.image.clone(),
                mapper_name: crate::volume_crypto::mapper_name_for_serial(&v.serial),
                dek,
            });
        }
        Ok(out)
    }

    /// Look a VM up by id, falling back to its name the way the other
    /// operator-facing RPCs do.
    fn resolve_vm(&self, vm_id: &str) -> Result<VmRow, Status> {
        let key = vm_id.trim();
        if key.is_empty() {
            return Err(Status::invalid_argument("vm_id is required"));
        }
        self.db
            .get_vm(key)
            .map_err(|e| Status::internal(e.to_string()))?
            .or_else(|| {
                self.db
                    .list_vms()
                    .ok()
                    .and_then(|rows| rows.into_iter().find(|v| v.name == key))
            })
            .ok_or_else(|| Status::not_found(format!("VM '{key}' not found")))
    }

    /// Nodes to ask about a receive session.
    ///
    /// An explicit `node_id` wins. Otherwise every CephCluster member is
    /// queried, because live migrate only ever prepares a receive on one of
    /// those, and an operator chasing an `ALREADY_EXISTS` does not necessarily
    /// know yet which node is holding it.
    fn receive_status_targets(&self, node_id: &str) -> Result<Vec<NodeRow>, Status> {
        let all = self
            .db
            .list_nodes()
            .map_err(|e| Status::internal(e.to_string()))?;
        let key = node_id.trim();
        if !key.is_empty() {
            let node = all
                .into_iter()
                .find(|n| n.id == key || n.address == key)
                .ok_or_else(|| Status::not_found(format!("node '{key}' not found")))?;
            return Ok(vec![node]);
        }
        Ok(all
            .into_iter()
            .filter(|n| self.node_supports_backend(n, "ceph"))
            .collect())
    }

    /// Ask one node for its receive bookkeeping. `None` when the node could
    /// not be reached, which the caller reports rather than treats as fatal.
    async fn fetch_receive_state(
        &self,
        node: &NodeRow,
        runtime_name: &str,
    ) -> Result<Option<controller_proto::LiveMigrateReceiveState>, Status> {
        let mut admin = self.ensure_admin_client_for_node(node).await?;
        let resp = admin
            .get_live_migrate_receive_status(node_proto::GetLiveMigrateReceiveStatusRequest {
                vm_name: runtime_name.to_string(),
            })
            .await
            .map_err(|e| {
                status_with_context(
                    &e,
                    &format!(
                        "reading live-migrate receive state for '{runtime_name}' on node {}",
                        node.id
                    ),
                )
            })?
            .into_inner();
        Ok(resp.state.as_ref().map(receive_state_from_node))
    }

    /// Per-node row for `GetLiveMigrateReceiveStatus`, with an unreachable
    /// node recorded as an error instead of failing the whole report.
    async fn receive_status_for_node(
        &self,
        node: &NodeRow,
        vm: &VmRow,
        runtime_name: &str,
    ) -> controller_proto::NodeLiveMigrateReceiveStatus {
        let mut row = controller_proto::NodeLiveMigrateReceiveStatus {
            node_id: node.id.clone(),
            node_address: node.address.clone(),
            owns_vm: node.id == vm.node_id,
            ..Default::default()
        };
        match self.fetch_receive_state(node, runtime_name).await {
            Ok(state) => {
                row.reachable = true;
                row.state = state;
            }
            Err(e) => {
                row.reachable = false;
                row.error = e.message().to_string();
            }
        }
        row
    }

    /// Keep only the nodes that belong to a healthy CephCluster.
    fn healthy_ceph_members(&self, nodes: &[NodeRow]) -> Result<Vec<NodeRow>, Status> {
        let mut healthy = Vec::with_capacity(nodes.len());
        for node in nodes {
            if self.is_healthy_ceph_member(&node.id)? {
                healthy.push(node.clone());
            }
        }
        Ok(healthy)
    }

    /// Guest-visible flags that must not disappear across a live migration.
    /// Host virtualization bits (`vmx`/`svm`) are not in this set.
    fn cpu_flags_missing_on_dest(source: &[String], dest: &[String]) -> Vec<String> {
        const FLAGS: &[&str] = &[
            "sse4_1",
            "sse4_2",
            "ssse3",
            "popcnt",
            "aes",
            "avx",
            "avx2",
            "xsave",
            "pclmulqdq",
        ];
        let have: std::collections::HashSet<&str> = dest.iter().map(String::as_str).collect();
        FLAGS
            .iter()
            .copied()
            .filter(|flag| source.iter().any(|got| got == flag) && !have.contains(flag))
            .map(str::to_string)
            .collect()
    }

    async fn ensure_live_migrate_cpu(
        &self,
        source_admin: &mut node_proto::node_admin_client::NodeAdminClient<
            tonic::transport::Channel,
        >,
        dest_admin: &mut node_proto::node_admin_client::NodeAdminClient<tonic::transport::Channel>,
        source: &NodeRow,
        target: &NodeRow,
    ) -> Result<(), Status> {
        let source_flags = match source_admin
            .get_host_cpu_flags(node_proto::GetHostCpuFlagsRequest {})
            .await
        {
            Ok(resp) => resp.into_inner().flags,
            Err(e)
                if e.code() == tonic::Code::Unimplemented
                    || e.code() == tonic::Code::Unavailable =>
            {
                return Ok(());
            }
            Err(e) => return Err(e),
        };
        if source_flags.is_empty() {
            return Ok(());
        }
        let dest_flags = match dest_admin
            .get_host_cpu_flags(node_proto::GetHostCpuFlagsRequest {})
            .await
        {
            Ok(resp) => resp.into_inner().flags,
            Err(e) if e.code() == tonic::Code::Unimplemented => return Ok(()),
            Err(e) => return Err(e),
        };
        let missing = Self::cpu_flags_missing_on_dest(&source_flags, &dest_flags);
        if missing.is_empty() {
            Ok(())
        } else {
            Err(Status::failed_precondition(format!(
                "cannot live-migrate from {} to {}: destination CPU is missing {}",
                source.id,
                target.id,
                missing.join(", ")
            )))
        }
    }

    fn local_fail(operation_id: impl Into<String>, status: Status) -> LiveMigrateFailure {
        LiveMigrateFailure {
            send_succeeded: false,
            status,
            operation_id: operation_id.into(),
        }
    }

    fn begin_local_op(
        &self,
        vm: &VmRow,
        source: &NodeRow,
        target: &NodeRow,
        phase: &str,
    ) -> Result<String, LiveMigrateFailure> {
        if let Ok(Some(open)) = self.db.get_open_vm_operation(&vm.id) {
            return Err(Self::local_fail(
                open.id.clone(),
                Status::failed_precondition(format!(
                    "VM '{}' already has open operation {} in phase {}",
                    vm.name, open.id, open.phase
                )),
            ));
        }
        let op_id = Uuid::new_v4().to_string();
        if let Err(e) = self.db.insert_vm_operation(&VmOperationRow {
            id: op_id.clone(),
            vm_id: vm.id.clone(),
            kind: VmOperationRow::KIND_LIVE_MIGRATE.into(),
            phase: phase.to_string(),
            source_node: source.id.clone(),
            target_node: target.id.clone(),
            cancel_requested: false,
            send_succeeded: false,
            detail_json: "{}".into(),
            started_at: String::new(),
            updated_at: String::new(),
            finished_at: String::new(),
        }) {
            return Err(Self::local_fail(
                op_id,
                Status::internal(format!("recording migrate operation: {e}")),
            ));
        }
        Ok(op_id)
    }

    fn close_local_op(&self, op_id: &str, phase: &str) {
        let _ = self
            .db
            .update_vm_operation_phase(op_id, phase, Some(false), None, true);
    }

    async fn node_has_local_migrate(&self, node: &NodeRow) -> Result<bool, Status> {
        let mut admin = self.ensure_admin_client_for_node(node).await?;
        match admin
            .get_ch_vm_module_features(node_proto::GetChVmModuleFeaturesRequest {})
            .await
        {
            Ok(resp) => Ok(resp
                .into_inner()
                .features
                .iter()
                .any(|feature| feature == crate::local_migrate::FEATURE)),
            Err(e) if e.code() == tonic::Code::Unimplemented => Ok(false),
            Err(e) => Err(e),
        }
    }

    /// Refuse an LVM destination that has already reported too little free
    /// space. A node that does not implement `GetLvmInfo` is left to `lvcreate`.
    async fn ensure_lvm_room(&self, node: &NodeRow, need_bytes: i64) -> Result<(), Status> {
        let mut admin = self.ensure_admin_client_for_node(node).await?;
        let info = match admin.get_lvm_info(node_proto::GetLvmInfoRequest {}).await {
            Ok(resp) => resp.into_inner(),
            Err(e) if e.code() == tonic::Code::Unimplemented => return Ok(()),
            Err(e) => {
                warn!(node = %node.id, error = %e, "GetLvmInfo failed; lvcreate will report space");
                return Ok(());
            }
        };
        if !info.available {
            return Ok(());
        }
        let Some(group) = info
            .volume_groups
            .iter()
            .find(|group| group.name == crate::local_migrate::LVM_VG)
        else {
            return Ok(());
        };
        if group.free_bytes < need_bytes {
            return Err(Status::failed_precondition(format!(
                "volume group {} on {} has {} bytes free, need {need_bytes}",
                crate::local_migrate::LVM_VG,
                node.id,
                group.free_bytes
            )));
        }
        Ok(())
    }

    async fn abort_volume_on(
        &self,
        node: &NodeRow,
        vm_name: &str,
        destroy_volume: bool,
    ) -> Result<(), Status> {
        let mut admin = self.ensure_admin_client_for_node(node).await?;
        let resp = admin
            .abort_volume_mirror(node_proto::AbortVolumeMirrorRequest {
                vm_name: vm_name.to_string(),
                destroy_volume,
            })
            .await
            .map_err(|e| {
                status_with_context(
                    &e,
                    &format!("aborting volume mirror for {vm_name} on {}", node.id),
                )
            })?
            .into_inner();
        if resp.success {
            Ok(())
        } else {
            Err(Status::internal(format!(
                "abort volume mirror on {}: {}",
                node.id, resp.message
            )))
        }
    }

    async fn release_volume_on(
        &self,
        node: &NodeRow,
        vm_name: &str,
        role: &str,
    ) -> Result<(), Status> {
        let mut admin = self.ensure_admin_client_for_node(node).await?;
        let resp = admin
            .release_volume_mirror(node_proto::ReleaseVolumeMirrorRequest {
                vm_name: vm_name.to_string(),
                role: role.to_string(),
            })
            .await
            .map_err(|e| {
                status_with_context(
                    &e,
                    &format!(
                        "releasing volume mirror ({role}) for {vm_name} on {}",
                        node.id
                    ),
                )
            })?
            .into_inner();
        if resp.success {
            Ok(())
        } else {
            Err(Status::internal(format!(
                "release volume mirror on {}: {}",
                node.id, resp.message
            )))
        }
    }

    async fn set_local_quiesced(
        &self,
        node: &NodeRow,
        vm_name: &str,
        quiesced: bool,
        start: bool,
    ) -> Result<(), Status> {
        let mut admin = self.ensure_admin_client_for_node(node).await?;
        let resp = admin
            .set_local_vm_quiesced(node_proto::SetLocalVmQuiescedRequest {
                vm_name: vm_name.to_string(),
                quiesced,
                start,
            })
            .await
            .map_err(|e| {
                status_with_context(
                    &e,
                    &format!("changing quiesce for {vm_name} on {}", node.id),
                )
            })?
            .into_inner();
        if resp.success {
            Ok(())
        } else {
            Err(Status::failed_precondition(resp.message))
        }
    }

    /// Move an LVM or ZFS VM by copying its block device, then either live
    /// migrating memory or reassigning the stopped guest.
    ///
    /// The destination copy is destroyed only when this attempt still owns it
    /// and the guest has not been sent. After a successful send both copies
    /// are kept until the source copy is explicitly released.
    async fn migrate_local_disk(
        &self,
        vm: &VmRow,
        source: &NodeRow,
        target: &NodeRow,
        allow_cold: bool,
    ) -> Result<(LiveMigrateOutcome, &'static str), LiveMigrateFailure> {
        if !self.node_supports_backend(source, &vm.storage_backend) {
            return Err(Self::local_fail(
                "",
                Status::failed_precondition(format!(
                    "source node {} uses storage backend {}, which cannot host this {} VM",
                    source.id, source.storage_backend, vm.storage_backend
                )),
            ));
        }
        if !self.node_supports_backend(target, &vm.storage_backend) {
            return Err(Self::local_fail(
                "",
                Status::failed_precondition(format!(
                    "target node {} uses storage backend {}, which cannot host this {} VM",
                    target.id, target.storage_backend, vm.storage_backend
                )),
            ));
        }
        for node in [source, target] {
            match self.node_has_local_migrate(node).await {
                Ok(true) => {}
                Ok(false) => {
                    return Err(Self::local_fail(
                        "",
                        Status::failed_precondition(format!(
                            "node {} does not advertise local disk migration ({}); upgrade it and apply its configuration",
                            node.id,
                            crate::local_migrate::FEATURE
                        )),
                    ));
                }
                Err(status) => return Err(Self::local_fail("", status)),
            }
        }

        let runtime_name = sanitize_nix_attr_key(&vm.name);
        let mut source_admin = self
            .ensure_admin_client_for_node(source)
            .await
            .map_err(|status| Self::local_fail("", status))?;
        let inspected = source_admin
            .inspect_local_vm_disk(node_proto::InspectLocalVmDiskRequest {
                vm_name: runtime_name.clone(),
                storage_backend: vm.storage_backend.clone(),
                vg_name: crate::local_migrate::LVM_VG.into(),
                pool_name: crate::local_migrate::ZFS_POOL.into(),
            })
            .await
            .map_err(|e| {
                Self::local_fail(
                    "",
                    status_with_context(&e, &format!("inspecting disk of VM '{}'", vm.name)),
                )
            })?
            .into_inner();
        if !inspected.success || inspected.size_bytes <= 0 {
            return Err(Self::local_fail(
                "",
                Status::failed_precondition(format!(
                    "VM '{}' has no local disk on {}: {}",
                    vm.name, source.id, inspected.message
                )),
            ));
        }
        let plan = crate::local_migrate::plan_local_move(
            inspected.running,
            inspected.guest_uses_mapper,
            inspected.memory_shared,
            allow_cold,
        )
        .map_err(|message| Self::local_fail("", Status::failed_precondition(message)))?;
        if vm.storage_backend == "lvm" {
            self.ensure_lvm_room(target, inspected.size_bytes)
                .await
                .map_err(|status| Self::local_fail("", status))?;
        }

        match plan {
            crate::local_migrate::LocalMove::Live => {
                self.live_mirror_local_disk(vm, source, target, &runtime_name, inspected.size_bytes)
                    .await
            }
            crate::local_migrate::LocalMove::ColdCopy => {
                self.cold_copy_local_disk(vm, source, target, &runtime_name, inspected.size_bytes)
                    .await
            }
        }
    }

    async fn prepare_mirror_target(
        &self,
        target: &NodeRow,
        vm: &VmRow,
        runtime_name: &str,
        size_bytes: i64,
    ) -> Result<node_proto::PrepareVolumeMirrorTargetResponse, Status> {
        let mut dest_admin = self.ensure_admin_client_for_node(target).await?;
        let prepared = dest_admin
            .prepare_volume_mirror_target(node_proto::PrepareVolumeMirrorTargetRequest {
                vm_name: runtime_name.to_string(),
                storage_backend: vm.storage_backend.clone(),
                size_bytes,
                bind_addr: grpc_address_host(&target.address),
                vg_name: crate::local_migrate::LVM_VG.into(),
                pool_name: crate::local_migrate::ZFS_POOL.into(),
            })
            .await
            .map_err(|e| {
                status_with_context(
                    &e,
                    &format!("preparing mirror target for '{}' on {}", vm.name, target.id),
                )
            })?
            .into_inner();
        if !prepared.success || prepared.listen_port <= 0 {
            return Err(Status::internal(format!(
                "prepare mirror target failed: {}",
                prepared.message
            )));
        }
        Ok(prepared)
    }

    async fn live_mirror_local_disk(
        &self,
        vm: &VmRow,
        source: &NodeRow,
        target: &NodeRow,
        runtime_name: &str,
        size_bytes: i64,
    ) -> Result<(LiveMigrateOutcome, &'static str), LiveMigrateFailure> {
        let op_id =
            self.begin_local_op(vm, source, target, VmOperationRow::PHASE_MIRROR_SYNCING)?;
        let prepared = match self
            .prepare_mirror_target(target, vm, runtime_name, size_bytes)
            .await
        {
            Ok(prepared) => prepared,
            Err(status) => {
                if let Err(abort) = self.abort_volume_on(target, runtime_name, true).await {
                    warn!(vm = %vm.name, error = %abort, "mirror target cleanup after prepare failure");
                }
                self.close_local_op(&op_id, VmOperationRow::PHASE_FAILED);
                return Err(Self::local_fail(op_id, status));
            }
        };
        let mut source_admin = match self.ensure_admin_client_for_node(source).await {
            Ok(admin) => admin,
            Err(status) => {
                let _ = self.abort_volume_on(target, runtime_name, true).await;
                self.close_local_op(&op_id, VmOperationRow::PHASE_FAILED);
                return Err(Self::local_fail(op_id, status));
            }
        };
        if let Err(e) = source_admin
            .start_volume_mirror(node_proto::StartVolumeMirrorRequest {
                vm_name: runtime_name.to_string(),
                target_host: prepared.listen_addr.clone(),
                target_port: prepared.listen_port,
                export_name: prepared.export_name.clone(),
                storage_backend: vm.storage_backend.clone(),
                vg_name: crate::local_migrate::LVM_VG.into(),
                pool_name: crate::local_migrate::ZFS_POOL.into(),
            })
            .await
        {
            let _ = self.abort_volume_on(source, runtime_name, false).await;
            let _ = self.abort_volume_on(target, runtime_name, true).await;
            self.close_local_op(&op_id, VmOperationRow::PHASE_FAILED);
            return Err(Self::local_fail(
                op_id,
                status_with_context(&e, &format!("starting disk mirror for '{}'", vm.name)),
            ));
        }

        let mut mirror_guard = LiveMirrorGuard::arm(
            self.clone(),
            source.clone(),
            target.clone(),
            runtime_name.to_string(),
            op_id.clone(),
        );
        let started = std::time::Instant::now();
        let limit = Duration::from_secs(6 * 60 * 60);
        let mut status_errors = 0u32;
        loop {
            if started.elapsed() > limit {
                mirror_guard.disarm();
                let _ = self.abort_volume_on(source, runtime_name, false).await;
                let _ = self.abort_volume_on(target, runtime_name, true).await;
                self.close_local_op(&op_id, VmOperationRow::PHASE_FAILED);
                return Err(Self::local_fail(
                    op_id,
                    Status::deadline_exceeded(format!(
                        "disk mirror of '{}' did not reach sync within 6h",
                        vm.name
                    )),
                ));
            }
            let cancel = self
                .db
                .get_vm_operation(&op_id)
                .ok()
                .flatten()
                .is_some_and(|op| op.cancel_requested);
            if cancel {
                mirror_guard.disarm();
                let restored = self.abort_volume_on(source, runtime_name, false).await;
                let _ = self.abort_volume_on(target, runtime_name, true).await;
                if let Err(status) = restored {
                    return Err(Self::local_fail(
                        op_id,
                        status_with_context(
                            &status,
                            "cancelled, but the source mirror could not be restored",
                        ),
                    ));
                }
                self.close_local_op(&op_id, VmOperationRow::PHASE_CANCELLED);
                return Err(Self::local_fail(
                    op_id,
                    Status::cancelled(format!("disk mirror of '{}' cancelled", vm.name)),
                ));
            }
            match source_admin
                .get_volume_mirror_status(node_proto::GetVolumeMirrorStatusRequest {
                    vm_name: runtime_name.to_string(),
                })
                .await
            {
                Ok(resp) => {
                    status_errors = 0;
                    let status = resp.into_inner();
                    let detail = crate::local_migrate::mirror_detail(
                        status.bytes_synced,
                        status.bytes_total,
                        status.in_sync,
                    );
                    let _ = self.db.update_vm_operation_phase(
                        &op_id,
                        VmOperationRow::PHASE_MIRROR_SYNCING,
                        None,
                        Some(&detail),
                        false,
                    );
                    if !status.success || status.degraded {
                        mirror_guard.disarm();
                        let _ = self.abort_volume_on(source, runtime_name, false).await;
                        let _ = self.abort_volume_on(target, runtime_name, true).await;
                        self.close_local_op(&op_id, VmOperationRow::PHASE_FAILED);
                        return Err(Self::local_fail(
                            op_id,
                            Status::failed_precondition(format!(
                                "disk mirror of '{}' became unusable ({})",
                                vm.name, status.message
                            )),
                        ));
                    }
                    if status.in_sync {
                        break;
                    }
                }
                Err(e) => {
                    status_errors += 1;
                    warn!(vm = %vm.name, error = %e, "volume mirror status");
                    if status_errors >= 3 {
                        mirror_guard.disarm();
                        let _ = self.abort_volume_on(source, runtime_name, false).await;
                        let _ = self.abort_volume_on(target, runtime_name, true).await;
                        self.close_local_op(&op_id, VmOperationRow::PHASE_FAILED);
                        return Err(Self::local_fail(
                            op_id,
                            status_with_context(&e, "reading disk mirror status"),
                        ));
                    }
                    if let Ok(fresh) = self.ensure_admin_client_for_node(source).await {
                        source_admin = fresh;
                    }
                }
            }
            tokio::time::sleep(Duration::from_secs(1)).await;
        }

        let mut dest_admin = match self.ensure_admin_client_for_node(target).await {
            Ok(admin) => admin,
            Err(status) => {
                mirror_guard.disarm();
                let _ = self.abort_volume_on(source, runtime_name, false).await;
                let _ = self.abort_volume_on(target, runtime_name, true).await;
                self.close_local_op(&op_id, VmOperationRow::PHASE_FAILED);
                return Err(Self::local_fail(op_id, status));
            }
        };
        if let Err(e) = dest_admin
            .commit_volume_mirror_target(node_proto::CommitVolumeMirrorTargetRequest {
                vm_name: runtime_name.to_string(),
            })
            .await
        {
            mirror_guard.disarm();
            let _ = self.abort_volume_on(source, runtime_name, false).await;
            let _ = self.abort_volume_on(target, runtime_name, true).await;
            self.close_local_op(&op_id, VmOperationRow::PHASE_FAILED);
            return Err(Self::local_fail(
                op_id,
                status_with_context(&e, "writing the destination seed marker"),
            ));
        }

        // Memory send has its own cleanup. A drop from here on must not tear
        // the mirror down out from under Cloud Hypervisor.
        mirror_guard.disarm();
        let dest_host = grpc_address_host(&target.address);
        match self
            .live_migrate_vm(vm, source, target, None, runtime_name, &dest_host)
            .await
        {
            Ok(mut outcome) => {
                if let Err(e) = self.release_volume_on(target, runtime_name, "target").await {
                    warn!(vm = %vm.name, error = %e, "destination NBD export still running");
                    outcome.warnings.push(format!(
                        "destination NBD export for '{}' was not stopped: {e}",
                        vm.name
                    ));
                }
                if let Err(e) = self.release_volume_on(source, runtime_name, "source").await {
                    warn!(vm = %vm.name, error = %e, "source disk was not deleted");
                    outcome
                        .warnings
                        .push(format!("source disk of '{}' was not deleted: {e}", vm.name));
                }
                Ok((outcome, "live"))
            }
            Err(err) if err.send_succeeded => {
                if let Err(e) = self.release_volume_on(target, runtime_name, "target").await {
                    warn!(vm = %vm.name, error = %e, "stop destination NBD after send");
                }
                if let Err(e) = self.abort_volume_on(source, runtime_name, false).await {
                    warn!(vm = %vm.name, error = %e, "restore source linear after send");
                }
                Err(err)
            }
            Err(err) => {
                match self.abort_volume_on(source, runtime_name, false).await {
                    Ok(()) => {
                        let _ = self.abort_volume_on(target, runtime_name, true).await;
                    }
                    Err(status) => {
                        warn!(
                            vm = %vm.name,
                            error = %status,
                            "source mirror still active after a failed send; destination copy kept"
                        );
                    }
                }
                Err(err)
            }
        }
    }

    async fn cold_copy_local_disk(
        &self,
        vm: &VmRow,
        source: &NodeRow,
        target: &NodeRow,
        runtime_name: &str,
        size_bytes: i64,
    ) -> Result<(LiveMigrateOutcome, &'static str), LiveMigrateFailure> {
        let op_id = self.begin_local_op(vm, source, target, VmOperationRow::PHASE_COPYING)?;
        if let Err(status) = self
            .set_local_quiesced(source, runtime_name, true, false)
            .await
        {
            let _ = self
                .set_local_quiesced(source, runtime_name, false, true)
                .await;
            self.close_local_op(&op_id, VmOperationRow::PHASE_FAILED);
            return Err(Self::local_fail(op_id, status));
        }
        let prepared = match self
            .prepare_mirror_target(target, vm, runtime_name, size_bytes)
            .await
        {
            Ok(prepared) => prepared,
            Err(status) => {
                let _ = self.abort_volume_on(target, runtime_name, true).await;
                let _ = self
                    .set_local_quiesced(source, runtime_name, false, true)
                    .await;
                self.close_local_op(&op_id, VmOperationRow::PHASE_FAILED);
                return Err(Self::local_fail(op_id, status));
            }
        };

        let stop_heartbeat = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let heartbeat_stop = std::sync::Arc::clone(&stop_heartbeat);
        let heartbeat_db = self.db.clone();
        let heartbeat_op = op_id.clone();
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_secs(30)).await;
                if heartbeat_stop.load(std::sync::atomic::Ordering::Relaxed) {
                    break;
                }
                let _ = heartbeat_db.update_vm_operation_phase(
                    &heartbeat_op,
                    VmOperationRow::PHASE_COPYING,
                    None,
                    None,
                    false,
                );
            }
        });
        let mut source_admin = match self.ensure_admin_client_for_node(source).await {
            Ok(admin) => admin,
            Err(status) => {
                stop_heartbeat.store(true, std::sync::atomic::Ordering::Relaxed);
                let _ = self.abort_volume_on(target, runtime_name, true).await;
                let _ = self
                    .set_local_quiesced(source, runtime_name, false, true)
                    .await;
                self.close_local_op(&op_id, VmOperationRow::PHASE_FAILED);
                return Err(Self::local_fail(op_id, status));
            }
        };
        let copied = source_admin
            .copy_local_volume(node_proto::CopyLocalVolumeRequest {
                vm_name: runtime_name.to_string(),
                storage_backend: vm.storage_backend.clone(),
                vg_name: crate::local_migrate::LVM_VG.into(),
                pool_name: crate::local_migrate::ZFS_POOL.into(),
                target_host: prepared.listen_addr,
                target_port: prepared.listen_port,
                export_name: prepared.export_name,
            })
            .await;
        stop_heartbeat.store(true, std::sync::atomic::Ordering::Relaxed);
        if let Err(e) = copied {
            let _ = self.abort_volume_on(target, runtime_name, true).await;
            let _ = self
                .set_local_quiesced(source, runtime_name, false, true)
                .await;
            self.close_local_op(&op_id, VmOperationRow::PHASE_FAILED);
            return Err(Self::local_fail(
                op_id,
                status_with_context(&e, &format!("copying disk of '{}'", vm.name)),
            ));
        }
        let cancel = self
            .db
            .get_vm_operation(&op_id)
            .ok()
            .flatten()
            .is_some_and(|op| op.cancel_requested);
        if cancel {
            let _ = self.abort_volume_on(source, runtime_name, false).await;
            let _ = self.abort_volume_on(target, runtime_name, true).await;
            let _ = self
                .set_local_quiesced(source, runtime_name, false, true)
                .await;
            self.close_local_op(&op_id, VmOperationRow::PHASE_CANCELLED);
            return Err(Self::local_fail(
                op_id,
                Status::cancelled(format!("disk copy of '{}' cancelled", vm.name)),
            ));
        }

        let mut dest_admin = match self.ensure_admin_client_for_node(target).await {
            Ok(admin) => admin,
            Err(status) => {
                let _ = self.abort_volume_on(source, runtime_name, false).await;
                let _ = self.abort_volume_on(target, runtime_name, true).await;
                let _ = self
                    .set_local_quiesced(source, runtime_name, false, true)
                    .await;
                self.close_local_op(&op_id, VmOperationRow::PHASE_FAILED);
                return Err(Self::local_fail(op_id, status));
            }
        };
        if let Err(e) = dest_admin
            .commit_volume_mirror_target(node_proto::CommitVolumeMirrorTargetRequest {
                vm_name: runtime_name.to_string(),
            })
            .await
        {
            let _ = self.abort_volume_on(source, runtime_name, false).await;
            let _ = self.abort_volume_on(target, runtime_name, true).await;
            let _ = self
                .set_local_quiesced(source, runtime_name, false, true)
                .await;
            self.close_local_op(&op_id, VmOperationRow::PHASE_FAILED);
            return Err(Self::local_fail(
                op_id,
                status_with_context(&e, "writing the destination seed marker"),
            ));
        }
        if let Err(status) = self.release_volume_on(target, runtime_name, "target").await {
            let _ = self.abort_volume_on(source, runtime_name, false).await;
            let _ = self.abort_volume_on(target, runtime_name, true).await;
            let _ = self
                .set_local_quiesced(source, runtime_name, false, true)
                .await;
            self.close_local_op(&op_id, VmOperationRow::PHASE_FAILED);
            return Err(Self::local_fail(op_id, status));
        }

        if let Err(status) = self.cold_reassign_vm(vm, source, target).await {
            let moved = self
                .db
                .get_vm(&vm.id)
                .ok()
                .flatten()
                .is_some_and(|row| row.node_id == target.id);
            if !moved {
                let _ = self.abort_volume_on(source, runtime_name, false).await;
                let _ = self.abort_volume_on(target, runtime_name, true).await;
                let _ = self
                    .set_local_quiesced(source, runtime_name, false, true)
                    .await;
            }
            self.close_local_op(&op_id, VmOperationRow::PHASE_FAILED);
            return Err(Self::local_fail(op_id, status));
        }
        if let Err(e) = self.release_volume_on(source, runtime_name, "source").await {
            warn!(vm = %vm.name, error = %e, "source disk was not deleted after cold copy");
        }
        let _ = self
            .set_local_quiesced(source, runtime_name, false, false)
            .await;
        self.close_local_op(&op_id, VmOperationRow::PHASE_DONE);
        Ok((
            LiveMigrateOutcome {
                operation_id: op_id,
                warnings: Vec::new(),
            },
            "cold",
        ))
    }

    async fn respond_local_migrate(
        &self,
        actor: &str,
        vm: &VmRow,
        source: &NodeRow,
        target: &NodeRow,
        allow_cold: bool,
    ) -> Result<Response<controller_proto::MigrateVmResponse>, Status> {
        match self
            .migrate_local_disk(vm, source, target, allow_cold)
            .await
        {
            Ok((outcome, mode)) => {
                self.webhooks.emit(
                    crate::webhooks::EVENT_VM_MIGRATED,
                    format!("vm/{}", vm.id),
                    serde_json::json!({
                        "vmId": vm.id,
                        "vmName": vm.name,
                        "sourceNode": source.id,
                        "targetNode": target.id,
                        "mode": mode,
                    }),
                );
                self.log_replication_event_required(
                    actor,
                    Some("MigrateVm"),
                    EVT_VM_MIGRATE,
                    &format!("vm/{}", vm.id),
                    serde_json::json!({
                        "vmId": vm.id,
                        "vmName": vm.name,
                        "sourceNode": source.id,
                        "targetNode": target.id,
                        "mode": mode,
                        "operationId": outcome.operation_id,
                        "hostWarnings": outcome.warnings,
                    }),
                )?;
                let mut message = format!(
                    "{mode}-migrated '{}' from {} to {}",
                    vm.name, source.id, target.id
                );
                for line in &outcome.warnings {
                    message.push_str("\nwarning: ");
                    message.push_str(line);
                }
                Ok(Response::new(controller_proto::MigrateVmResponse {
                    success: true,
                    message,
                    mode: mode.into(),
                    source_node: source.id.clone(),
                    target_node: target.id.clone(),
                    operation_id: outcome.operation_id,
                }))
            }
            Err(err) => {
                warn!(
                    vm = %vm.name,
                    send_succeeded = err.send_succeeded,
                    operation_id = %err.operation_id,
                    error = %err.status,
                    "local disk migrate failed"
                );
                Err(err.status)
            }
        }
    }

    async fn live_migrate_vm(
        &self,
        vm: &VmRow,
        source: &NodeRow,
        target: &NodeRow,
        volume: Option<&VolumeRow>,
        runtime_name: &str,
        dest_host: &str,
    ) -> Result<LiveMigrateOutcome, LiveMigrateFailure> {
        // A local-disk move records MirrorSyncing/Copying before calling in.
        // That row is this attempt, not a conflict.
        let local_disk = volume.is_none();
        let op_id = if let Ok(Some(open)) = self.db.get_open_vm_operation(&vm.id) {
            if local_disk && crate::local_migrate::local_phase_is_open(&open.phase) {
                open.id
            } else {
                return Err(LiveMigrateFailure {
                    send_succeeded: false,
                    status: Status::failed_precondition(format!(
                        "VM '{}' already has open operation {} in phase {}",
                        vm.name, open.id, open.phase
                    )),
                    operation_id: open.id,
                });
            }
        } else {
            let op_id = Uuid::new_v4().to_string();
            if let Err(e) = self.db.insert_vm_operation(&VmOperationRow {
                id: op_id.clone(),
                vm_id: vm.id.clone(),
                kind: VmOperationRow::KIND_LIVE_MIGRATE.into(),
                phase: VmOperationRow::PHASE_PREPARING.into(),
                source_node: source.id.clone(),
                target_node: target.id.clone(),
                cancel_requested: false,
                send_succeeded: false,
                detail_json: "{}".into(),
                started_at: String::new(),
                updated_at: String::new(),
                finished_at: String::new(),
            }) {
                return Err(LiveMigrateFailure {
                    send_succeeded: false,
                    status: Status::internal(format!("recording migrate operation: {e}")),
                    operation_id: op_id,
                });
            }
            op_id
        };
        if local_disk {
            let _ = self.db.update_vm_operation_phase(
                &op_id,
                VmOperationRow::PHASE_PREPARING,
                Some(false),
                None,
                false,
            );
        }
        let (rbd_volumes, encrypted_volumes, root_pool, root_image) = if let Some(volume) = volume {
            let attached =
                self.db
                    .list_attached_volumes_for_vm(&vm.id)
                    .map_err(|e| LiveMigrateFailure {
                        send_succeeded: false,
                        status: Status::internal(e.to_string()),
                        operation_id: op_id.clone(),
                    })?;
            let rbd_volumes: Vec<node_proto::RbdImageRef> = if attached.is_empty() {
                vec![node_proto::RbdImageRef {
                    pool: volume.pool.clone(),
                    image: volume.image.clone(),
                }]
            } else {
                attached
                    .iter()
                    .map(|v| node_proto::RbdImageRef {
                        pool: v.pool.clone(),
                        image: v.image.clone(),
                    })
                    .collect()
            };
            let encrypted_volumes = self
                .encrypted_volume_keys_for_vols(if attached.is_empty() {
                    std::slice::from_ref(volume)
                } else {
                    attached.as_slice()
                })
                .map_err(|status| LiveMigrateFailure {
                    send_succeeded: false,
                    status,
                    operation_id: op_id.clone(),
                })?;
            let root_pool = rbd_volumes
                .first()
                .map(|v| v.pool.clone())
                .unwrap_or_else(|| volume.pool.clone());
            let root_image = rbd_volumes
                .first()
                .map(|v| v.image.clone())
                .unwrap_or_else(|| volume.image.clone());
            (rbd_volumes, encrypted_volumes, root_pool, root_image)
        } else {
            (Vec::new(), Vec::new(), String::new(), String::new())
        };
        let mut dest_admin = self
            .ensure_admin_client_for_node(target)
            .await
            .map_err(|status| LiveMigrateFailure {
                send_succeeded: false,
                status,
                operation_id: op_id.clone(),
            })?;
        let mut source_admin =
            self.ensure_admin_client_for_node(source)
                .await
                .map_err(|status| LiveMigrateFailure {
                    send_succeeded: false,
                    status,
                    operation_id: op_id.clone(),
                })?;
        if let Err(status) = self
            .ensure_live_migrate_cpu(&mut source_admin, &mut dest_admin, source, target)
            .await
        {
            let _ = self.db.update_vm_operation_phase(
                &op_id,
                VmOperationRow::PHASE_FAILED,
                Some(false),
                None,
                true,
            );
            return Err(LiveMigrateFailure {
                send_succeeded: false,
                status,
                operation_id: op_id,
            });
        }
        // The destination compares this against itself inside prepare. An
        // agent without the RPC leaves it unset and the comparison is skipped.
        let source_cpu = match source_admin
            .get_host_cpu_info(node_proto::GetHostCpuInfoRequest {})
            .await
        {
            Ok(resp) => resp.into_inner().cpu,
            Err(e) => {
                warn!(node = %source.id, error = %e, "source host CPU info unavailable; skipping host comparison");
                None
            }
        };
        self.push_config_to_node_inner(target, true, Some(vm))
            .await
            .map_err(|status| {
                let _ = self.db.update_vm_operation_phase(
                    &op_id,
                    VmOperationRow::PHASE_FAILED,
                    Some(false),
                    None,
                    true,
                );
                LiveMigrateFailure {
                    send_succeeded: false,
                    status,
                    operation_id: op_id.clone(),
                }
            })?;
        // The destination rebuild restarts its agent. Reconnect before prepare.
        dest_admin = self
            .ensure_admin_client_for_node(target)
            .await
            .map_err(|status| {
                let _ = self.db.update_vm_operation_phase(
                    &op_id,
                    VmOperationRow::PHASE_FAILED,
                    Some(false),
                    None,
                    true,
                );
                LiveMigrateFailure {
                    send_succeeded: false,
                    status,
                    operation_id: op_id.clone(),
                }
            })?;

        let prep = match dest_admin
            .prepare_live_migrate_receive(node_proto::PrepareLiveMigrateReceiveRequest {
                vm_name: runtime_name.to_string(),
                rbd_pool: root_pool.clone(),
                rbd_image: root_image.clone(),
                listen_port: 0,
                rbd_volumes: rbd_volumes.clone(),
                encrypted_volumes: encrypted_volumes.clone(),
                source_cpu,
                local_disk,
            })
            .await
        {
            Ok(resp) => resp.into_inner(),
            Err(e) => {
                let _ = dest_admin
                    .abort_live_migrate_receive(node_proto::AbortLiveMigrateReceiveRequest {
                        vm_name: runtime_name.to_string(),
                        rbd_pool: root_pool.clone(),
                        rbd_image: root_image.clone(),
                        rbd_volumes: rbd_volumes.clone(),
                    })
                    .await;
                let _ = self.db.update_vm_operation_phase(
                    &op_id,
                    VmOperationRow::PHASE_FAILED,
                    Some(false),
                    None,
                    true,
                );
                return Err(LiveMigrateFailure {
                    send_succeeded: false,
                    status: status_with_context(
                        &e,
                        &format!("preparing node {} to receive VM '{}'", target.id, vm.name),
                    ),
                    operation_id: op_id,
                });
            }
        };
        if !prep.success || prep.listen_port <= 0 {
            let _ = self.db.update_vm_operation_phase(
                &op_id,
                VmOperationRow::PHASE_FAILED,
                Some(false),
                None,
                true,
            );
            return Err(LiveMigrateFailure {
                send_succeeded: false,
                status: Status::internal(format!("prepare receive failed: {}", prep.message)),
                operation_id: op_id,
            });
        }
        let host_warnings = prep.warnings.clone();
        for line in &host_warnings {
            warn!(vm = %vm.name, source = %source.id, target = %target.id, "live migrate host difference: {line}");
        }

        if self
            .db
            .get_vm_operation(&op_id)
            .ok()
            .flatten()
            .map(|o| o.cancel_requested)
            .unwrap_or(false)
        {
            let _ = dest_admin
                .abort_live_migrate_receive(node_proto::AbortLiveMigrateReceiveRequest {
                    vm_name: runtime_name.to_string(),
                    rbd_pool: root_pool.clone(),
                    rbd_image: root_image.clone(),
                    rbd_volumes: rbd_volumes.clone(),
                })
                .await;
            let _ = self.db.update_vm_operation_phase(
                &op_id,
                VmOperationRow::PHASE_CANCELLED,
                Some(false),
                None,
                true,
            );
            return Err(LiveMigrateFailure {
                send_succeeded: false,
                status: Status::cancelled(format!(
                    "live migrate of '{}' cancelled before send",
                    vm.name
                )),
                operation_id: op_id,
            });
        }

        let _ = self.db.update_vm_operation_phase(
            &op_id,
            VmOperationRow::PHASE_SENDING,
            None,
            None,
            false,
        );
        // The destination knows which of its addresses the migration listener
        // is reachable on; only fall back to the host part of its gRPC address
        // when it declines to say (empty or a wildcard bind).
        let dial_host = migration_dial_host(&prep.listen_addr, dest_host);
        let destination_url = format!("tcp:{dial_host}:{}", prep.listen_port);

        let send_result = source_admin
            .send_live_migrate(node_proto::SendLiveMigrateRequest {
                vm_name: runtime_name.to_string(),
                destination_url: destination_url.clone(),
                timeout_seconds: 600,
            })
            .await;

        if let Err(e) = send_result {
            let _ = dest_admin
                .abort_live_migrate_receive(node_proto::AbortLiveMigrateReceiveRequest {
                    vm_name: runtime_name.to_string(),
                    rbd_pool: root_pool.clone(),
                    rbd_image: root_image.clone(),
                    rbd_volumes: rbd_volumes.clone(),
                })
                .await;
            let _ = self.db.update_vm_operation_phase(
                &op_id,
                VmOperationRow::PHASE_FAILED,
                Some(false),
                None,
                true,
            );
            return Err(LiveMigrateFailure {
                send_succeeded: false,
                status: status_with_context(
                    &e,
                    &format!(
                        "sending VM '{}' from node {} to {destination_url}",
                        vm.name, source.id
                    ),
                ),
                operation_id: op_id,
            });
        }

        let _ = self.db.update_vm_operation_phase(
            &op_id,
            VmOperationRow::PHASE_WAITING,
            Some(true),
            None,
            false,
        );
        // After a successful send, the source VMM is gone — do not abort the
        // destination receive session on wait errors (that would kill the only
        // remaining guest process).
        let wait = dest_admin
            .wait_live_migrate_receive(node_proto::WaitLiveMigrateReceiveRequest {
                vm_name: runtime_name.to_string(),
                // Match send timeout: receive may still be flushing after send returns.
                timeout_seconds: 600,
            })
            .await
            .map_err(|e| {
                let _ = self.db.update_vm_operation_phase(
                    &op_id,
                    VmOperationRow::PHASE_FAILED,
                    Some(true),
                    None,
                    true,
                );
                LiveMigrateFailure {
                    send_succeeded: true,
                    status: status_with_context(
                        &e,
                        &format!(
                            "waiting for VM '{}' to finish arriving on node {}",
                            vm.name, target.id
                        ),
                    ),
                    operation_id: op_id.clone(),
                }
            })?
            .into_inner();
        if !wait.success {
            let _ = self.db.update_vm_operation_phase(
                &op_id,
                VmOperationRow::PHASE_FAILED,
                Some(true),
                None,
                true,
            );
            return Err(LiveMigrateFailure {
                send_succeeded: true,
                status: Status::internal(format!(
                    "node {} did not complete the receive for VM '{}': {}",
                    target.id, vm.name, wait.message
                )),
                operation_id: op_id,
            });
        }

        let _ = self.db.update_vm_operation_phase(
            &op_id,
            VmOperationRow::PHASE_REASSIGNED,
            Some(true),
            None,
            false,
        );
        self.reassign_vm_node(vm, &target.id).map_err(|status| {
            let _ = self.db.update_vm_operation_phase(
                &op_id,
                VmOperationRow::PHASE_FAILED,
                Some(true),
                None,
                true,
            );
            LiveMigrateFailure {
                send_succeeded: true,
                status,
                operation_id: op_id.clone(),
            }
        })?;

        self.push_config_and_await_apply(source)
            .await
            .map_err(|status| {
                let _ = self.db.update_vm_operation_phase(
                    &op_id,
                    VmOperationRow::PHASE_FAILED,
                    Some(true),
                    None,
                    true,
                );
                LiveMigrateFailure {
                    send_succeeded: true,
                    status: status_with_context(
                        &status,
                        &format!(
                            "removing VM '{}' from node {} after live migrate",
                            vm.name, source.id
                        ),
                    ),
                    operation_id: op_id.clone(),
                }
            })?;
        // `finalize_live_migrate_dest` starts the generated VM unit, so the
        // destination rebuild has to have activated before we ask for it.
        let _ = self.db.update_vm_operation_phase(
            &op_id,
            VmOperationRow::PHASE_FINALIZING_DEST,
            Some(true),
            None,
            false,
        );
        self.push_config_and_await_apply(target)
            .await
            .map_err(|status| {
                let _ = self.db.update_vm_operation_phase(
                    &op_id,
                    VmOperationRow::PHASE_FAILED,
                    Some(true),
                    None,
                    true,
                );
                LiveMigrateFailure {
                    send_succeeded: true,
                    status,
                    operation_id: op_id.clone(),
                }
            })?;

        dest_admin
            .finalize_live_migrate_dest(node_proto::FinalizeLiveMigrateDestRequest {
                vm_name: runtime_name.to_string(),
            })
            .await
            .map_err(|e| {
                let _ = self.db.update_vm_operation_phase(
                    &op_id,
                    VmOperationRow::PHASE_FAILED,
                    Some(true),
                    None,
                    true,
                );
                LiveMigrateFailure {
                    send_succeeded: true,
                    status: status_with_context(
                        &e,
                        &format!(
                            "adopting migrated VM '{}' into systemd on node {}",
                            vm.name, target.id
                        ),
                    ),
                    operation_id: op_id.clone(),
                }
            })?;

        let _ = self.db.update_vm_operation_phase(
            &op_id,
            VmOperationRow::PHASE_FINALIZING_SOURCE,
            Some(true),
            None,
            false,
        );
        // The guest already runs on the destination. Retry the source release;
        // a source that keeps the RBD mapped will start a second writer on its
        // next boot if its unit is still installed.
        let mut released = false;
        let mut last_detail = String::new();
        for attempt in 1..=3 {
            match source_admin
                .finalize_live_migrate_source(node_proto::FinalizeLiveMigrateSourceRequest {
                    vm_name: runtime_name.to_string(),
                    rbd_pool: root_pool.clone(),
                    rbd_image: root_image.clone(),
                    rbd_volumes: rbd_volumes.clone(),
                })
                .await
            {
                Ok(resp) => {
                    let resp = resp.into_inner();
                    if resp.vmm_stopped && resp.rbd_unmapped {
                        released = true;
                        break;
                    }
                    last_detail = format!(
                        "vmm_stopped={} rbd_unmapped={} {}",
                        resp.vmm_stopped, resp.rbd_unmapped, resp.message
                    );
                }
                Err(e) => {
                    last_detail = e.to_string();
                    if let Ok(fresh) = self.ensure_admin_client_for_node(source).await {
                        source_admin = fresh;
                    }
                }
            }
            if attempt < 3 {
                tokio::time::sleep(Duration::from_secs(2)).await;
            }
        }
        if !released {
            let _ = self.db.update_vm_operation_phase(
                &op_id,
                VmOperationRow::PHASE_FAILED,
                Some(true),
                None,
                true,
            );
            return Err(LiveMigrateFailure {
                send_succeeded: true,
                status: Status::failed_precondition(format!(
                    "VM '{}' is on {} but {} did not release the RBD image: {last_detail}",
                    vm.name, target.id, source.id
                )),
                operation_id: op_id,
            });
        }

        let _ = self.db.update_vm_operation_phase(
            &op_id,
            VmOperationRow::PHASE_DONE,
            Some(true),
            None,
            true,
        );
        Ok(LiveMigrateOutcome {
            operation_id: op_id,
            warnings: host_warnings,
        })
    }

    /// Stop a Ceph-backed VM and unmap its RBD image on the node that owns it
    /// *before* anything else touches that image — another node mapping it
    /// (cold move) or the pool deleting it (`DeleteVm`).
    ///
    /// Config pushes trigger an asynchronous `nixos-rebuild` on each node, so a
    /// cold move that only rewrote configs would let the destination map and
    /// boot from the shared RBD while the source VMM was still writing to it.
    /// `rbd unmap` cannot succeed while a local VMM holds the device open, so
    /// the node's *observed* `rbd_unmapped` is positive proof the source has
    /// let go — the call is the exclusivity barrier, not just a request.
    ///
    /// An unreachable source node is tolerated (that is the node-failure drain
    /// case, where the source VMM is gone with the node); anything else — a
    /// failed call, or a success whose post-conditions are false — is a hard
    /// error rather than a risk of two writers.
    async fn cold_release_ceph_vm(&self, vm: &VmRow, source: &NodeRow) -> Result<(), Status> {
        if vm.storage_backend != "ceph" {
            return Ok(());
        }
        let attached = self
            .db
            .list_attached_volumes_for_vm(&vm.id)
            .map_err(|e| Status::internal(e.to_string()))?;
        let rbd_volumes: Vec<node_proto::RbdImageRef> = attached
            .iter()
            .map(|v| node_proto::RbdImageRef {
                pool: v.pool.clone(),
                image: v.image.clone(),
            })
            .collect();
        let (pool, image) = match rbd_volumes.first() {
            Some(v) => (v.pool.clone(), v.image.clone()),
            None => {
                warn!(
                    vm = %vm.name,
                    node = %source.id,
                    "Ceph VM has no volume row; RBD release can only be verified via the stopped unit"
                );
                (String::new(), String::new())
            }
        };
        let runtime_name = sanitize_nix_attr_key(&vm.name);
        let mut admin = match self.ensure_admin_client_for_node(source).await {
            Ok(c) => c,
            Err(status) if status.code() == tonic::Code::Unavailable => {
                warn!(
                    node = %source.id,
                    vm = %vm.name,
                    error = %status,
                    "source node unreachable; skipping RBD release barrier"
                );
                return Ok(());
            }
            Err(status) => return Err(status),
        };
        match admin
            .finalize_live_migrate_source(node_proto::FinalizeLiveMigrateSourceRequest {
                vm_name: runtime_name,
                rbd_pool: pool,
                rbd_image: image,
                rbd_volumes,
            })
            .await
        {
            Ok(resp) => check_release_barrier(&vm.name, &source.id, &resp.into_inner()),
            Err(e) if e.code() == tonic::Code::Unavailable => {
                warn!(
                    node = %source.id,
                    vm = %vm.name,
                    error = %e,
                    "source node unreachable during RBD release barrier"
                );
                Ok(())
            }
            Err(e) => Err(Status::failed_precondition(format!(
                "could not release shared RBD for VM '{}' on node {}: {e}; refusing to touch the \
                 shared image while the source may still be writing",
                vm.name, source.id
            ))),
        }
    }

    async fn cold_reassign_vm(
        &self,
        vm: &VmRow,
        source: &NodeRow,
        target: &NodeRow,
    ) -> Result<(), Status> {
        self.cold_release_ceph_vm(vm, source).await?;
        self.reassign_vm_node(vm, &target.id)?;
        // The source has already provably released the image, so its rebuild
        // is bookkeeping and need not be waited on. The destination's is not:
        // the caller reports the move as done, which is only true once the new
        // unit exists there.
        if let Err(e) = self.push_config_to_node(source).await {
            warn!(node = %source.id, error = %e, "push after cold migrate (source)");
        }
        self.push_config_and_await_apply(target).await?;
        Ok(())
    }

    /// Hand a VM's ownership to another node.
    ///
    /// A single `UPDATE` of `node_id`, not delete-then-reinsert. Every table
    /// keyed on `vms(id)` cascades on delete, so the old implementation quietly
    /// destroyed each migrated VM's `security_group_vm_attachments` rows (SSH
    /// keys were restored by hand; security groups were not). Updating in place
    /// removes the whole class of bug — and keeps `created_at`, which a
    /// reassignment has no business resetting.
    /// GPUs currently bound to this VM, in assignment order.
    fn vm_gpu_names(&self, vm_id: &str) -> Result<Vec<String>, Status> {
        let stored = self
            .db
            .get_vm_gpu(vm_id)
            .map_err(|e| Status::internal(format!("reading GPU assignment: {e}")))?
            .unwrap_or_default();
        Ok(stored
            .split(',')
            .map(str::trim)
            .filter(|name| !name.is_empty())
            .map(str::to_string)
            .collect())
    }

    fn node_gpu_inventory(&self, node_id: &str) -> Result<Vec<kcore_gpu::GpuDevice>, Status> {
        let rows = self
            .db
            .list_node_gpus(Some(node_id))
            .map_err(|e| Status::internal(format!("listing GPUs on {node_id}: {e}")))?;
        Ok(rows
            .into_iter()
            .map(|row| kcore_gpu::GpuDevice {
                name: row.name,
                family: row.family,
                model: row.model,
                address: row.address,
                functions: crate::pci::split_pci_devices(&row.pci_devices),
                iommu_group: row.iommu_group,
                assignable: row.assignable,
                blocked_reason: row.blocked_reason,
                kind: if row.kind.is_empty() {
                    "gpu".into()
                } else {
                    row.kind
                },
                role: row.role,
                class_code: row.class_code,
                characteristics: row.characteristics,
                driver: row.driver,
                parent: row.parent,
                profile: row.profile,
                profiles: row
                    .profiles
                    .split(',')
                    .map(str::trim)
                    .filter(|profile| !profile.is_empty())
                    .map(str::to_string)
                    .collect(),
            })
            .collect())
    }

    fn taken_gpu_names(
        &self,
        node_id: &str,
        except_vm: Option<&str>,
    ) -> Result<std::collections::HashSet<String>, Status> {
        let vms = self
            .db
            .list_vms_for_node(node_id)
            .map_err(|e| Status::internal(format!("listing VMs on {node_id}: {e}")))?;
        let inventory = self.node_gpu_inventory(node_id)?;
        let mut taken = std::collections::HashSet::new();
        for vm in vms {
            if except_vm == Some(vm.id.as_str()) {
                continue;
            }
            for name in self.vm_gpu_names(&vm.id)? {
                taken.insert(name);
            }
            let owned = crate::pci::split_pci_devices(&vm.pci_devices);
            for gpu in &inventory {
                if gpu
                    .functions
                    .iter()
                    .any(|addr| owned.iter().any(|have| have == addr))
                {
                    taken.insert(gpu.name.clone());
                }
            }
        }
        Ok(taken)
    }

    /// Resolve operator GPU names against one node's inventory and store the result.
    fn assign_gpus_on_node(
        &self,
        node_id: &str,
        requests: &[String],
        except_vm: Option<&str>,
    ) -> Result<(String, String), Status> {
        let selectors = requests
            .iter()
            .map(|raw| kcore_gpu::parse_selector(raw).map_err(Status::invalid_argument))
            .collect::<Result<Vec<_>, _>>()?;
        let inventory = self.node_gpu_inventory(node_id)?;
        let taken = self.taken_gpu_names(node_id, except_vm)?;
        let chosen = kcore_gpu::select_gpus(&inventory, &selectors, &taken)
            .map_err(Status::failed_precondition)?;
        let names = chosen
            .iter()
            .map(|gpu| gpu.name.clone())
            .collect::<Vec<_>>()
            .join(",");
        let mut functions = Vec::new();
        for gpu in &chosen {
            functions.extend(gpu.functions.iter().cloned());
        }
        let pci_devices =
            crate::pci::join_pci_devices(&functions).map_err(Status::invalid_argument)?;
        Ok((names, pci_devices))
    }

    /// Resolve vGPU requests against one node's inventory. Stored as `name=profile`.
    fn assign_vgpus_on_node(
        &self,
        node_id: &str,
        requests: &[String],
        except_vm: Option<&str>,
    ) -> Result<(String, String), Status> {
        let selectors = requests
            .iter()
            .map(|raw| kcore_gpu::parse_vgpu_selector(raw).map_err(Status::invalid_argument))
            .collect::<Result<Vec<_>, _>>()?;
        let inventory = self.node_gpu_inventory(node_id)?;
        let taken = self.taken_gpu_names(node_id, except_vm)?;
        let chosen = kcore_gpu::select_vgpus(&inventory, &selectors, &taken)
            .map_err(Status::failed_precondition)?;
        let names = chosen
            .iter()
            .map(|(gpu, profile)| format!("{}={profile}", gpu.name))
            .collect::<Vec<_>>()
            .join(",");
        let mut functions = Vec::new();
        for (gpu, _) in &chosen {
            functions.extend(gpu.functions.iter().cloned());
        }
        let pci_devices =
            crate::pci::join_pci_devices(&functions).map_err(Status::invalid_argument)?;
        Ok((names, pci_devices))
    }

    fn vgpu_slices_for(
        &self,
        node_id: &str,
        vms: &[VmRow],
    ) -> Result<Vec<nixgen::VgpuSliceNix>, Status> {
        let inventory = self.node_gpu_inventory(node_id)?;
        let mut slices = Vec::new();
        for vm in vms {
            if vm.node_id != node_id {
                continue;
            }
            let names = self.vm_gpu_names(&vm.id)?;
            let addrs = crate::pci::split_pci_devices(&vm.pci_devices);
            for (name, addr) in names.iter().zip(addrs.iter()) {
                let Some(profile) = kcore_gpu::binding_profile(name) else {
                    continue;
                };
                let device = kcore_gpu::binding_device_name(name);
                let host_address = inventory
                    .iter()
                    .find(|row| row.name == device)
                    .and_then(|slice| inventory.iter().find(|host| host.name == slice.parent))
                    .map(|host| host.address.clone())
                    .unwrap_or_default();
                slices.push(nixgen::VgpuSliceNix {
                    vm_id: vm.id.clone(),
                    address: addr.clone(),
                    profile: profile.to_string(),
                    host_address,
                });
            }
        }
        Ok(slices)
    }

    fn store_gpu_assignment(
        &self,
        vm: &VmRow,
        names: &str,
        pci_devices: &str,
    ) -> Result<(), Status> {
        let previous_pci = vm.pci_devices.clone();
        self.db
            .set_vm_pci_devices(&vm.id, pci_devices)
            .map_err(|e| Status::internal(format!("storing PCI devices: {e}")))?;
        if let Err(e) = self.db.set_vm_gpu(&vm.id, names) {
            let _ = self.db.set_vm_pci_devices(&vm.id, &previous_pci);
            return Err(Status::internal(format!("storing GPU assignment: {e}")));
        }
        Ok(())
    }

    /// Point a GPU or vGPU VM at free or named devices on `target` and store that assignment.
    /// The VM's node id is left unchanged; the caller moves it.
    fn retarget_vm_gpus(
        &self,
        vm: &VmRow,
        target: &NodeRow,
        explicit: &[String],
        explicit_vgpus: &[String],
    ) -> Result<String, Status> {
        let current = self.vm_gpu_names(&vm.id)?;
        let has_slice = current.iter().any(|name| name.contains('='));
        if has_slice || explicit_vgpus.iter().any(|gpu| !gpu.trim().is_empty()) {
            if explicit.iter().any(|gpu| !gpu.trim().is_empty()) {
                return Err(Status::invalid_argument(format!(
                    "VM '{}' has vGPU slices; migrate it with --vgpu, not --gpu",
                    vm.name
                )));
            }
            let requests = if explicit_vgpus.iter().any(|gpu| !gpu.trim().is_empty()) {
                explicit_vgpus
                    .iter()
                    .map(|gpu| gpu.trim().to_string())
                    .filter(|gpu| !gpu.is_empty())
                    .collect::<Vec<_>>()
            } else if current.is_empty() {
                return Err(Status::failed_precondition(format!(
                    "VM '{}' has PCI devices ({}) and no vGPU profile; migrate it with --vgpu A2-4C",
                    vm.name, vm.pci_devices
                )));
            } else {
                current
                    .iter()
                    .map(|name| {
                        kcore_gpu::binding_profile(name)
                            .map(str::to_string)
                            .ok_or_else(|| {
                                Status::failed_precondition(format!(
                                    "VM '{}' slice '{name}' has no profile; migrate it with --vgpu",
                                    vm.name
                                ))
                            })
                    })
                    .collect::<Result<Vec<_>, _>>()?
            };
            let (names, pci_devices) =
                self.assign_vgpus_on_node(&target.id, &requests, Some(&vm.id))?;
            self.store_gpu_assignment(vm, &names, &pci_devices)?;
            return Ok(names);
        }
        let requests = if explicit.iter().any(|gpu| !gpu.trim().is_empty()) {
            explicit
                .iter()
                .map(|gpu| gpu.trim().to_string())
                .filter(|gpu| !gpu.is_empty())
                .collect::<Vec<_>>()
        } else {
            let current = self.vm_gpu_names(&vm.id)?;
            if current.is_empty() {
                return Err(Status::failed_precondition(format!(
                    "VM '{}' has PCI devices ({}) and no device name; migrate it with --gpu radeon0 (or another name from `kctl pci list`)",
                    vm.name, vm.pci_devices
                )));
            }
            current
                .iter()
                .map(|name| {
                    kcore_gpu::family_of_name(name).map(str::to_string).ok_or_else(|| {
                        Status::failed_precondition(format!(
                            "VM '{}' device '{name}' has no family; migrate it with --gpu, --nic, or --nvme from `kctl pci list`",
                            vm.name
                        ))
                    })
                })
                .collect::<Result<Vec<_>, _>>()?
        };
        let (names, pci_devices) = self.assign_gpus_on_node(&target.id, &requests, Some(&vm.id))?;
        self.store_gpu_assignment(vm, &names, &pci_devices)?;
        Ok(names)
    }

    fn restore_vm_gpu(&self, vm_id: &str, pci_devices: &str, gpu_name: &str) {
        let _ = self.db.set_vm_pci_devices(vm_id, pci_devices);
        if gpu_name.is_empty() {
            let _ = self.db.clear_vm_gpu(vm_id);
        } else {
            let _ = self.db.set_vm_gpu(vm_id, gpu_name);
        }
    }

    async fn migrate_gpu_vm(
        &self,
        actor: &str,
        vm: VmRow,
        target_node: &str,
        gpus: &[String],
        vgpus: &[String],
        ignore_admission: bool,
    ) -> Result<Response<controller_proto::MigrateVmResponse>, Status> {
        let source_node = self
            .db
            .get_node(&vm.node_id)
            .map_err(|e| Status::internal(e.to_string()))?
            .ok_or_else(|| Status::not_found(format!("source node '{}' not found", vm.node_id)))?;
        let all_nodes = self
            .db
            .list_nodes()
            .map_err(|e| Status::internal(e.to_string()))?;
        let target = all_nodes
            .iter()
            .find(|n| n.id == target_node || n.address == target_node)
            .ok_or_else(|| Status::not_found(format!("target node '{target_node}' not found")))?
            .clone();
        if target.id == source_node.id {
            return Err(Status::invalid_argument(
                "target_node must differ from the VM's current node",
            ));
        }
        accepts_migrated_vms(&target)?;
        self.admit_move(actor, &vm, &target.id, ignore_admission)?;
        if !self.node_supports_backend(&target, &vm.storage_backend) {
            return Err(Status::failed_precondition(format!(
                "target node does not support storage backend '{}'",
                vm.storage_backend
            )));
        }
        let previous_pci = vm.pci_devices.clone();
        let previous_names = self.vm_gpu_names(&vm.id)?.join(",");
        let names = self.retarget_vm_gpus(&vm, &target, gpus, vgpus)?;
        if let Err(e) = self.cold_release_ceph_vm(&vm, &source_node).await {
            self.restore_vm_gpu(&vm.id, &previous_pci, &previous_names);
            return Err(e);
        }
        if let Err(e) = self.reassign_vm_node(&vm, &target.id) {
            self.restore_vm_gpu(&vm.id, &previous_pci, &previous_names);
            return Err(e);
        }
        if let Err(e) = self.push_config_to_node(&source_node).await {
            warn!(node = %source_node.id, error = %e, "push after GPU migrate (source)");
        }
        self.push_config_and_await_apply(&target).await?;
        let from = if previous_names.is_empty() {
            previous_pci
        } else {
            previous_names
        };
        self.webhooks.emit(
            crate::webhooks::EVENT_VM_MIGRATED,
            format!("vm/{}", vm.id),
            serde_json::json!({
                "vmId": vm.id,
                "vmName": vm.name,
                "sourceNode": source_node.id,
                "targetNode": target.id,
                "mode": "cold",
            }),
        );
        self.log_replication_event_required(
            actor,
            Some("MigrateVm"),
            EVT_VM_MIGRATE,
            &format!("vm/{}", vm.id),
            serde_json::json!({
                "vmId": vm.id,
                "vmName": vm.name,
                "sourceNode": source_node.id,
                "targetNode": target.id,
                "mode": "cold",
                "gpus": names,
            }),
        )?;
        Ok(Response::new(controller_proto::MigrateVmResponse {
            success: true,
            message: format!(
                "cold-migrated '{}' from {} to {} and assigned {names} (was {from}). {} are not live-migrated; the guest starts on the destination",
                vm.name,
                source_node.id,
                target.id,
                if names.contains('=') {
                    "vGPU slices"
                } else {
                    "Whole-device GPUs"
                }
            ),
            mode: "cold".into(),
            source_node: source_node.id,
            target_node: target.id,
            operation_id: String::new(),
        }))
    }

    fn admission_report(
        &self,
        place: Option<(&VmRow, &str)>,
        extra: Option<(&str, i32, i64, &str, &str)>,
    ) -> Result<crate::ha::AdmissionReport, Status> {
        let nodes = self
            .db
            .list_nodes()
            .map_err(|e| Status::internal(format!("listing nodes: {e}")))?;
        let vms = self
            .db
            .list_vms()
            .map_err(|e| Status::internal(format!("listing vms: {e}")))?;
        let policies = self.db.list_vm_ha().unwrap_or_default();
        let mut hosts = Vec::new();
        let mut index = std::collections::HashMap::new();
        for node in &nodes {
            let (survivor, can_fail) = crate::ha::host_role(node);
            index.insert(node.id.clone(), hosts.len());
            hosts.push(crate::ha::AdmissionHost {
                cpu: crate::scheduler::schedulable_cpu(node.cpu_cores, self.overcommit.cpu),
                memory: crate::scheduler::schedulable_memory(
                    node.memory_bytes,
                    self.overcommit.memory,
                ),
                survivor,
                can_fail,
                domain: crate::ha::failure_domain_label(
                    &self.db.get_node_labels(&node.id).unwrap_or_default(),
                ),
            });
        }
        let mut admission_vms = Vec::new();
        for vm in &vms {
            let node_id = match place {
                Some((moving, target)) if moving.id == vm.id => target,
                _ => vm.node_id.as_str(),
            };
            let Some(&host_index) = index.get(node_id) else {
                continue;
            };
            let replica = policies
                .iter()
                .find(|row| row.vm_id == vm.id)
                .map(|row| row.replica_node.as_str())
                .unwrap_or("");
            admission_vms.push(crate::ha::AdmissionVm {
                host_index,
                cpu: i64::from(vm.cpu),
                memory: vm.memory_bytes,
                protected: crate::ha::is_protected(&vm.storage_backend, replica),
            });
        }
        if let Some((node_id, cpu, memory, backend, replica)) = extra {
            if let Some(&host_index) = index.get(node_id) {
                admission_vms.push(crate::ha::AdmissionVm {
                    host_index,
                    cpu: i64::from(cpu),
                    memory,
                    protected: crate::ha::is_protected(backend, replica),
                });
            }
        }
        Ok(crate::ha::admission(
            &hosts,
            &admission_vms,
            self.failover.admission_control.host_failures_to_tolerate,
        ))
    }

    fn enforce_admission(
        &self,
        report: &crate::ha::AdmissionReport,
        ignore: bool,
        actor: &str,
        resource: &str,
    ) -> Result<(), Status> {
        if report.capacity != crate::ha::CAPACITY_INSUFFICIENT {
            return Ok(());
        }
        if ignore {
            self.record_audit(
                actor,
                "IgnoreAdmissionControl",
                resource,
                report.message.clone(),
            );
            return Ok(());
        }
        Err(Status::failed_precondition(format!(
            "admission control: {}",
            report.message
        )))
    }

    fn ha_view(&self, vm: &VmRow) -> (String, String, String, i32, String) {
        let row = self.db.get_vm_ha(&vm.id).ok().flatten();
        let replica = row
            .as_ref()
            .map(|row| row.replica_node.clone())
            .unwrap_or_default();
        (
            crate::ha::protection(&vm.storage_backend, &replica).to_string(),
            row.as_ref()
                .map(|row| row.restart_priority.clone())
                .unwrap_or_else(|| crate::ha::PRIORITY_MEDIUM.to_string()),
            replica,
            row.as_ref()
                .map(|row| i32::try_from(row.replica_interval_secs).unwrap_or(0))
                .unwrap_or(0),
            row.as_ref()
                .map(|row| row.vm_monitoring.clone())
                .unwrap_or_else(|| "off".to_string()),
        )
    }

    fn admit_move(
        &self,
        actor: &str,
        vm: &VmRow,
        target_node_id: &str,
        ignore: bool,
    ) -> Result<(), Status> {
        let replica = self
            .db
            .get_vm_ha(&vm.id)
            .ok()
            .flatten()
            .map(|row| row.replica_node)
            .unwrap_or_default();
        if !crate::ha::is_protected(&vm.storage_backend, &replica) {
            return Ok(());
        }
        let report = self.admission_report(Some((vm, target_node_id)), None)?;
        self.enforce_admission(&report, ignore, actor, &format!("vm/{}", vm.id))
    }

    fn reassign_vm_node(&self, vm: &VmRow, target_node_id: &str) -> Result<(), Status> {
        let moved = self.db.set_vm_node(&vm.id, target_node_id).map_err(|e| {
            Status::internal(format!(
                "reassigning VM '{}' to node {target_node_id}: {e}",
                vm.name
            ))
        })?;
        if !moved {
            return Err(Status::not_found(format!(
                "VM '{}' no longer exists; cannot reassign it to node {target_node_id}",
                vm.name
            )));
        }
        Ok(())
    }

    /// Best-effort rollback after CreateVm partially succeeded (RBD + DB).
    async fn rollback_created_vm(&self, node: &NodeRow, vm: &VmRow) {
        if vm.storage_backend == "ceph" {
            if self.clients.get_storage(&node.address).is_none() {
                let _ = self.clients.connect(&node.address).await;
            }
            if let Some(mut storage) = self.clients.get_storage(&node.address) {
                let handle = format!("kcore-vms/kcore-{}", vm.id);
                if let Err(e) = storage
                    .delete_volume(node_proto::DeleteVolumeRequest {
                        backend_handle: handle,
                    })
                    .await
                {
                    warn!(vm_id = %vm.id, error = %e, "rollback: failed to delete RBD volume");
                }
            }
            if let Err(e) = self.db.delete_volume_by_vm(&vm.id) {
                warn!(vm_id = %vm.id, error = %e, "rollback: failed to delete volume row");
            }
        }
        if let Err(e) = self.db.delete_vm_by_id_or_name(&vm.id) {
            error!(vm_id = %vm.id, error = %e, "rollback: failed to delete VM row");
        }
    }

    async fn set_vm_desired_state_internal(
        &self,
        vm_id: &str,
        target_node: &str,
        auto_start: bool,
    ) -> Result<i32, Status> {
        let node = self.resolve_node_for_vm(vm_id, target_node)?;
        // Capture the original auto_start value so we can roll the DB back if
        // the node-side push fails. Without this, an idempotent CreateVm/
        // SetDesiredState that races with a node outage leaves the stored VM
        // matching the manifest while the node never reconciled — the next
        // apply returns UNCHANGED and silently swallows the failure.
        let stored_vm = self
            .db
            .get_vm(vm_id)
            .map_err(|e| Status::internal(format!("fetching vm: {e}")))?
            .or_else(|| {
                self.db
                    .list_vms_for_node(&node.id)
                    .ok()
                    .and_then(|rows| rows.into_iter().find(|vm| vm.name == vm_id))
            })
            .ok_or_else(|| Status::not_found(format!("VM {vm_id} not found")))?;
        let vm_name = stored_vm.name.clone();
        let original_auto_start = stored_vm.auto_start;
        let updated = self
            .db
            .set_vm_auto_start(vm_id, auto_start)
            .map_err(|e| Status::internal(format!("updating vm desired state: {e}")))?;
        if !updated {
            return Err(Status::not_found(format!("VM {vm_id} not found")));
        }
        // Await the rebuild: the whole point of the rollback below is that a
        // stored desired state the node never reconciled is worse than an
        // error, and a config push that only wrote a file cannot tell the
        // difference.
        if let Err(e) = self.push_config_and_await_apply(&node).await {
            // Roll back the desired_state mutation so retries see the stale
            // spec and re-trigger the reconcile on the next CreateVm/Apply.
            // Use compare-and-swap so a concurrent successful update is not
            // overwritten by this request's stale snapshot.
            if original_auto_start != auto_start {
                match self
                    .db
                    .set_vm_auto_start_if_current(vm_id, auto_start, original_auto_start)
                {
                    Ok(true) => {}
                    Ok(false) => {
                        debug!(
                            vm_id = %vm_id,
                            node_id = %node.id,
                            "skipped auto_start rollback after push failure; row changed concurrently"
                        );
                    }
                    Err(rb) => {
                        warn!(
                            vm_id = %vm_id,
                            node_id = %node.id,
                            error = %rb,
                            "failed to roll back vm auto_start after node push failure; DB may be inconsistent"
                        );
                    }
                }
            }
            return Err(e);
        }
        // The switch has applied autoStart. A stopped guest has no VMM
        // socket, so the next heartbeat cannot report it, and a stale
        // "running" value would start it on another host after a failure.
        let observed = if auto_start { "running" } else { "stopped" };
        if let Err(e) = self
            .db
            .update_vm_runtime_state(&node.id, &vm_name, observed)
        {
            warn!(
                node_id = %node.id,
                vm_id = %vm_id,
                error = %e,
                "applied desired state but could not record runtime state"
            );
        } else {
            self.replicate_vm_observation(&stored_vm.id, observed);
        }
        let desired_state = if auto_start {
            node_proto::VmDesiredState::Running as i32
        } else {
            node_proto::VmDesiredState::Stopped as i32
        };
        match self.ensure_compute_client_for_address(&node.address).await {
            Ok(mut compute) => {
                if let Err(e) = compute
                    .set_vm_desired_state(node_proto::SetVmDesiredStateRequest {
                        vm_id: vm_name,
                        desired_state,
                    })
                    .await
                {
                    warn!(
                        node_id = %node.id,
                        vm_id = %vm_id,
                        error = %e,
                        "failed to apply runtime desired state; declarative config already updated"
                    );
                }
            }
            Err(e) => {
                warn!(
                    node_id = %node.id,
                    vm_id = %vm_id,
                    error = %e,
                    "missing compute client for runtime desired state apply; declarative config already updated"
                );
            }
        }
        Ok(if auto_start {
            controller_proto::VmState::Running as i32
        } else {
            controller_proto::VmState::Stopped as i32
        })
    }

    /// Upsert path for `CreateVm`: compare incoming spec against the stored
    /// VM row, reject any immutable-field changes, and apply mutable-field
    /// changes (cpu, memory_bytes, desired_state) via the existing update and
    /// desired-state paths. Returns `UPDATED` (with the changed_fields list)
    /// or `UNCHANGED` when the incoming spec already matches storage.
    async fn upsert_existing_vm(
        &self,
        actor: &str,
        req: controller_proto::CreateVmRequest,
        spec: controller_proto::VmSpec,
        stored: VmRow,
    ) -> Result<Response<controller_proto::CreateVmResponse>, Status> {
        let stored_ssh = self
            .db
            .get_vm_ssh_key_names(&stored.id)
            .map_err(|e| Status::internal(format!("listing vm ssh keys: {e}")))?;

        let storage_backend_str = if req.storage_backend == 0 {
            String::new()
        } else {
            normalize_storage_backend(req.storage_backend, false)?
        };

        let image_url_trim = req.image_url.trim();
        let image_sha256_trim = req.image_sha256.trim();
        let image_path_trim = req.image_path.trim();
        let cloud_init_trim = req.cloud_init_user_data.trim();

        // Resolve incoming `target_node` (which may be a node id OR an
        // address like "host:port") to its canonical node id before diffing.
        // Without this, every re-apply that uses an address would trip the
        // immutable check ("target_node": "host:port" != "node-abc"), even
        // when the VM is sitting on exactly that node.
        let target_node_trim = req.target_node.trim();
        let resolved_target_node = if target_node_trim.is_empty() {
            String::new()
        } else {
            self.db
                .get_node_by_address(target_node_trim)
                .map_err(|e| Status::internal(e.to_string()))?
                .or_else(|| self.db.get_node(target_node_trim).ok().flatten())
                .map(|n| n.id)
                .unwrap_or_else(|| target_node_trim.to_string())
        };

        let apply = crate::grpc::diff::VmApply {
            spec: &spec,
            image_url: image_url_trim,
            image_sha256: image_sha256_trim,
            image_path: image_path_trim,
            cloud_init_user_data: cloud_init_trim,
            ssh_key_names: &req.ssh_key_names,
            storage_backend: &storage_backend_str,
            storage_size_bytes: req.storage_size_bytes,
            target_node: &resolved_target_node,
            target_dc: req.target_dc.trim(),
        };
        let mut diff = crate::grpc::diff::diff_vm(&stored, &stored_ssh, &apply);
        if spec.gpus.iter().any(|gpu| !gpu.trim().is_empty()) {
            let current = self.vm_gpu_names(&stored.id)?;
            let requested: Vec<String> = spec
                .gpus
                .iter()
                .map(|gpu| gpu.trim().to_string())
                .filter(|gpu| !gpu.is_empty())
                .collect();
            let same = requested.len() == current.len()
                && requested.iter().zip(&current).all(
                    |(want, have)| match kcore_gpu::parse_selector(want) {
                        Ok(kcore_gpu::GpuSelector::Exact(name)) => name == *have,
                        Ok(kcore_gpu::GpuSelector::Family(family)) => {
                            kcore_gpu::family_of_name(have).is_some_and(|fam| fam == family)
                        }
                        Err(_) => false,
                    },
                );
            if !same {
                diff.immutable.push("gpu".into());
            }
        }
        if spec.vgpus.iter().any(|gpu| !gpu.trim().is_empty()) {
            let current = self.vm_gpu_names(&stored.id)?;
            let requested: Vec<String> = spec
                .vgpus
                .iter()
                .map(|gpu| gpu.trim().to_string())
                .filter(|gpu| !gpu.is_empty())
                .collect();
            let same = requested.len() == current.len()
                && requested.iter().zip(&current).all(|(want, have)| {
                    match kcore_gpu::parse_vgpu_selector(want) {
                        Ok(kcore_gpu::VgpuSelector::Profile(profile)) => {
                            kcore_gpu::binding_profile(have) == Some(profile.as_str())
                        }
                        Ok(kcore_gpu::VgpuSelector::Card { card, profile }) => have
                            .strip_prefix(&format!("{card}/"))
                            .and_then(|rest| rest.split_once('='))
                            .is_some_and(|(_, got)| got == profile),
                        Ok(kcore_gpu::VgpuSelector::Slice { name, profile }) => {
                            have == &format!("{name}={profile}")
                        }
                        Err(_) => false,
                    }
                });
            if !same {
                diff.immutable.push("vgpu".into());
            }
        }
        if spec.nics.len() > 1 {
            let stored_extras = self
                .db
                .list_vm_nics(&stored.id)
                .map_err(|e| Status::internal(format!("listing extra NICs: {e}")))?;
            let incoming: Vec<String> = spec
                .nics
                .iter()
                .skip(1)
                .map(|nic| nic.network.trim().to_string())
                .collect();
            let stored_nets: Vec<String> = stored_extras
                .iter()
                .map(|nic| nic.network.clone())
                .collect();
            if incoming != stored_nets {
                return Err(Status::invalid_argument(format!(
                    "cannot change immutable field(s) on VM '{}': nics (delete the VM and recreate)",
                    stored.name
                )));
            }
        }
        let incoming_group = crate::scheduler::normalize_anti_affinity(&req.anti_affinity)
            .map_err(Status::invalid_argument)?;
        if !incoming_group.is_empty() {
            let stored_group = self
                .db
                .get_vm_anti_affinity(&stored.id)
                .map_err(|e| Status::internal(format!("reading anti-affinity: {e}")))?;
            if stored_group != incoming_group {
                return Err(Status::invalid_argument(format!(
                    "cannot change immutable field(s) on VM '{}': anti_affinity (delete the VM and recreate)",
                    stored.name
                )));
            }
        }

        if !diff.immutable.is_empty() {
            return Err(Status::invalid_argument(format!(
                "cannot change immutable field(s) on VM '{}': {} (delete the VM and recreate)",
                stored.name,
                diff.immutable.join(", ")
            )));
        }

        let current_state_enum = vm_state_from_runtime_str(&stored.runtime_state);

        if diff.mutable.is_empty() {
            return Ok(Response::new(controller_proto::CreateVmResponse {
                vm_id: stored.id,
                node_id: stored.node_id,
                state: current_state_enum,
                action: controller_proto::ApplyAction::Unchanged as i32,
                changed_fields: Vec::new(),
            }));
        }

        let mut changed_fields: Vec<String> = Vec::new();

        let cpu_changed = diff.mutable.iter().any(|f| f == "cpu");
        let mem_changed = diff.mutable.iter().any(|f| f == "memory_bytes");
        if cpu_changed || mem_changed {
            let cpu = if cpu_changed && spec.cpu > 0 {
                Some(spec.cpu)
            } else {
                None
            };
            let mem = if mem_changed && spec.memory_bytes > 0 {
                Some(spec.memory_bytes)
            } else {
                None
            };

            // Re-run placement preflight before accepting a larger shape so an
            // idempotent CreateVm cannot overcommit a node that the original
            // CreateVm path would have rejected.
            let node = self
                .db
                .get_node(&stored.node_id)
                .map_err(|e| Status::internal(format!("resolving node for update: {e}")))?
                .ok_or_else(|| Status::not_found(format!("node '{}' not found", stored.node_id)))?;
            // Build a preflight spec that reflects only the *delta* in cpu/mem
            // (existing reservations are already counted in node.cpu_used /
            // node.memory_used), so we only check headroom for the increase.
            let cpu_delta = cpu
                .map(|new_cpu| (new_cpu - stored.cpu).max(0))
                .unwrap_or(0);
            let mem_delta = mem
                .map(|new_mem| (new_mem - stored.memory_bytes).max(0))
                .unwrap_or(0);
            if cpu_delta > 0 || mem_delta > 0 {
                let preflight_spec = controller_proto::VmSpec {
                    cpu: cpu_delta,
                    memory_bytes: mem_delta,
                    ..spec.clone()
                };
                let backend_for_preflight = if storage_backend_str.is_empty() {
                    stored.storage_backend.as_str()
                } else {
                    storage_backend_str.as_str()
                };
                self.preflight_vm_create_on_node(&node, &preflight_spec, backend_for_preflight)?;
            }

            // Persist the new spec, then push declarative config to the node.
            // If the push fails, roll back the DB update so the next retry
            // sees the original spec and runs the reconcile again.
            let updated = self
                .db
                .update_vm_spec(&stored.id, cpu, mem)
                .map_err(|e| Status::internal(format!("updating vm spec: {e}")))?;
            if !updated {
                return Err(Status::not_found(format!(
                    "VM '{}' disappeared during upsert",
                    stored.id
                )));
            }
            if let Err(e) = self.push_config_to_node(&node).await {
                let rollback_cpu = cpu_changed.then_some(stored.cpu);
                let rollback_mem = mem_changed.then_some(stored.memory_bytes);
                if let Err(rollback_err) =
                    self.db
                        .update_vm_spec(&stored.id, rollback_cpu, rollback_mem)
                {
                    warn!(
                        vm_id = %stored.id,
                        node_id = %stored.node_id,
                        error = %rollback_err,
                        "failed to roll back vm spec after node push failure; DB may be inconsistent"
                    );
                }
                return Err(e);
            }
            if cpu_changed {
                changed_fields.push("cpu".into());
            }
            if mem_changed {
                changed_fields.push("memory_bytes".into());
            }

            self.log_replication_event(
                actor,
                Some("UpdateVm"),
                EVT_VM_UPDATE,
                &format!("vm/{}", stored.id),
                serde_json::json!({
                    "vmId": stored.id,
                    "nodeId": stored.node_id,
                    "cpu": cpu,
                    "memoryBytes": mem,
                }),
            );
        }

        let mut final_state_enum = current_state_enum;
        if diff.mutable.iter().any(|f| f == "desired_state") {
            let want_running = matches!(
                controller_proto::VmDesiredState::try_from(spec.desired_state)
                    .unwrap_or(controller_proto::VmDesiredState::Unspecified),
                controller_proto::VmDesiredState::Running,
            );
            final_state_enum = self
                .set_vm_desired_state_internal(&stored.id, "", want_running)
                .await?;
            changed_fields.push("desired_state".into());
            self.log_replication_event(
                actor,
                Some("SetVmDesiredState"),
                EVT_VM_DESIRED_STATE_SET,
                &format!("vm/{}", stored.id),
                serde_json::json!({
                    "vmId": stored.id,
                    "targetNode": "",
                    "autoStart": want_running,
                }),
            );
        }

        Ok(Response::new(controller_proto::CreateVmResponse {
            vm_id: stored.id,
            node_id: stored.node_id,
            state: final_state_enum,
            action: controller_proto::ApplyAction::Updated as i32,
            changed_fields,
        }))
    }

    /// Which newer ch-vm options this node's installed module accepts. A
    /// node agent predating the RPC, or a module without the feature file,
    /// gets the baseline so its evaluation keeps working.
    async fn ch_vm_module_features(
        &self,
        node: &NodeRow,
        admin: &mut node_proto::node_admin_client::NodeAdminClient<tonic::transport::Channel>,
    ) -> nixgen::ModuleFeatures {
        match admin
            .get_ch_vm_module_features(node_proto::GetChVmModuleFeaturesRequest {})
            .await
        {
            Ok(resp) => {
                let features = resp.into_inner().features;
                debug!(node = %node.id, ?features, "ch-vm module features reported by node");
                nixgen::ModuleFeatures::from_reported(&features)
            }
            Err(e) => {
                info!(
                    node = %node.id,
                    code = ?e.code(),
                    "node does not report ch-vm module features; emitting baseline options only"
                );
                nixgen::ModuleFeatures::default()
            }
        }
    }

    async fn ensure_admin_client_for_node(
        &self,
        node: &NodeRow,
    ) -> Result<node_proto::node_admin_client::NodeAdminClient<tonic::transport::Channel>, Status>
    {
        if let Some(client) = self.clients.get_admin(&node.address) {
            return Ok(client);
        }
        self.clients.connect(&node.address).await.map_err(|e| {
            Status::unavailable(format!("no connection to node {}: {e}", node.address))
        })?;
        self.clients
            .get_admin(&node.address)
            .ok_or_else(|| Status::unavailable(format!("no connection to node {}", node.address)))
    }

    /// Health as a surviving member reports it, for deciding whether a host
    /// may leave. `None` when no survivor answered.
    async fn ceph_health_via_survivor(
        &self,
        survivors: &[&controller_proto::CephClusterNodeSpec],
    ) -> Option<String> {
        for node in survivors {
            let Ok(Some(row)) = self.db.get_node(&node.node_id) else {
                continue;
            };
            let Ok(mut admin) = self.ensure_admin_client_for_node(&row).await else {
                continue;
            };
            match admin
                .get_ceph_health(node_proto::GetCephHealthRequest {
                    cluster_name: "ceph".into(),
                })
                .await
            {
                Ok(resp) => return Some(resp.into_inner().health_status),
                Err(e) => {
                    warn!(node = %node.node_id, error = %e, "ceph health via survivor failed");
                }
            }
        }
        None
    }

    /// Guard rails for the members `spec` drops from the stored cluster.
    async fn plan_ceph_member_removal(
        &self,
        name: &str,
        spec: &controller_proto::CephClusterSpec,
    ) -> Result<ceph_cluster_spec::RemovalPlan, Status> {
        let Some(existing) = self
            .db
            .get_ceph_cluster(name)
            .map_err(|e| Status::internal(e.to_string()))?
        else {
            return Ok(ceph_cluster_spec::RemovalPlan::default());
        };
        let old = ceph_cluster_spec::spec_from_json(&existing.spec_json)
            .map_err(|e| Status::internal(format!("decode stored ceph spec: {e}")))?;
        if ceph_cluster_spec::removed_members(&old, spec).is_empty() {
            return Ok(ceph_cluster_spec::RemovalPlan::default());
        }
        let survivors: Vec<&controller_proto::CephClusterNodeSpec> = spec
            .nodes
            .iter()
            .filter(|n| old.nodes.iter().any(|o| o.node_id == n.node_id))
            .collect();
        let health = self.ceph_health_via_survivor(&survivors).await;
        let pending = self
            .db
            .list_retired_ceph_nodes(name)
            .map_err(|e| Status::internal(e.to_string()))?;
        Ok(ceph_cluster_spec::plan_member_removal(
            &old,
            spec,
            &ceph_cluster_spec::RemovalContext {
                health_status: health.as_deref(),
                pending_removals: &pending,
            },
        ))
    }

    async fn plan_ceph_cluster_spec(
        &self,
        name: &str,
        spec: &controller_proto::CephClusterSpec,
    ) -> Result<controller_proto::PlanCephClusterResponse, Status> {
        ceph_cluster_spec::validate_spec(spec).map_err(Status::invalid_argument)?;
        let removal = self.plan_ceph_member_removal(name, spec).await?;
        let cluster_issues = removal.issues;
        let mut node_results = Vec::new();
        let mon_map = crate::ceph_cluster_reconciler::mon_map_string(spec);
        for (index, node) in spec.nodes.iter().enumerate() {
            let Some(node_row) = self
                .db
                .get_node(&node.node_id)
                .map_err(|e| Status::internal(e.to_string()))?
            else {
                node_results.push(controller_proto::CephNodePlanResult {
                    node_id: node.node_id.clone(),
                    ok: false,
                    issues: vec![format!("node '{}' is not registered", node.node_id)],
                });
                continue;
            };
            let daemon_id =
                crate::ceph_cluster_reconciler::sanitize_daemon_id(&node.node_id, index);
            let ceph_nix =
                crate::ceph_cluster_reconciler::generate_ceph_nix(spec, node, &daemon_id, &mon_map);
            let devices = ceph_cluster_spec::osd_devices(node);
            let mut admin = match self.ensure_admin_client_for_node(&node_row).await {
                Ok(c) => c,
                Err(e) => {
                    #[cfg(test)]
                    {
                        let _ = e;
                        node_results.push(controller_proto::CephNodePlanResult {
                            node_id: node.node_id.clone(),
                            ok: true,
                            issues: vec![
                                "preflight skipped in unit test (node agent unreachable)".into()
                            ],
                        });
                        continue;
                    }
                    #[cfg(not(test))]
                    {
                        node_results.push(controller_proto::CephNodePlanResult {
                            node_id: node.node_id.clone(),
                            ok: false,
                            issues: vec![e.message().to_string()],
                        });
                        continue;
                    }
                }
            };
            match admin
                .check_ceph_preflight(Request::new(node_proto::CheckCephPreflightRequest {
                    ceph_nix,
                    public_addr: node
                        .mon_addr
                        .split(':')
                        .next()
                        .unwrap_or(&node.mon_addr)
                        .to_string(),
                    cluster_addr: node.cluster_addr.clone(),
                    public_iface: node.public_iface.clone(),
                    cluster_iface: node.cluster_iface.clone(),
                    osd_devices: devices,
                    mon_addr: node.mon_addr.clone(),
                }))
                .await
            {
                Ok(resp) => {
                    let inner = resp.into_inner();
                    let issues: Vec<String> = inner
                        .issues
                        .into_iter()
                        .map(|i| {
                            format!(
                                "[{}]{} {}",
                                i.check,
                                if i.blocking { " BLOCKING" } else { "" },
                                i.message
                            )
                        })
                        .collect();
                    node_results.push(controller_proto::CephNodePlanResult {
                        node_id: node.node_id.clone(),
                        ok: inner.ok,
                        issues,
                    });
                }
                Err(e) => {
                    #[cfg(test)]
                    {
                        let _ = e;
                        node_results.push(controller_proto::CephNodePlanResult {
                            node_id: node.node_id.clone(),
                            ok: true,
                            issues: vec![
                                "preflight skipped in unit test (preflight rpc failed)".into()
                            ],
                        });
                    }
                    #[cfg(not(test))]
                    {
                        node_results.push(controller_proto::CephNodePlanResult {
                            node_id: node.node_id.clone(),
                            ok: false,
                            issues: vec![format!("preflight rpc failed: {e}")],
                        });
                    }
                }
            }
        }
        let ok = node_results.iter().all(|n| n.ok) && cluster_issues.is_empty();
        Ok(controller_proto::PlanCephClusterResponse {
            ok,
            encrypt_osds_notice: ceph_cluster_spec::encrypt_osds_notice(spec.encrypt_osds),
            nodes: node_results,
            cluster_issues,
            cluster_warnings: removal.warnings,
            removing_members: removal.removed,
        })
    }

    async fn ensure_compute_client_for_address(
        &self,
        address: &str,
    ) -> Result<node_proto::node_compute_client::NodeComputeClient<tonic::transport::Channel>, Status>
    {
        if let Some(client) = self.clients.get_compute(address) {
            return Ok(client);
        }
        self.clients
            .connect(address)
            .await
            .map_err(|e| Status::unavailable(format!("no connection to node {address}: {e}")))?;
        self.clients
            .get_compute(address)
            .ok_or_else(|| Status::unavailable(format!("no connection to node {address}")))
    }

    async fn ensure_container_client_for_address(
        &self,
        address: &str,
    ) -> Result<
        node_proto::node_container_client::NodeContainerClient<tonic::transport::Channel>,
        Status,
    > {
        if let Some(client) = self.clients.get_container(address) {
            return Ok(client);
        }
        self.clients
            .connect(address)
            .await
            .map_err(|e| Status::unavailable(format!("no connection to node {address}: {e}")))?;
        self.clients
            .get_container(address)
            .ok_or_else(|| Status::unavailable(format!("no connection to node {address}")))
    }

    fn log_replication_event(
        &self,
        actor: &str,
        audit_action: Option<&str>,
        event_type: &str,
        resource_key: &str,
        body: serde_json::Value,
    ) {
        if let Some(action) = audit_action {
            self.record_audit(actor, action, resource_key, "");
        }
        let Some(rep) = &self.replication else {
            return;
        };
        let logical_ts_unix_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as i64)
            .unwrap_or(0);
        let envelope = serde_json::json!({
            "schemaVersion": 1,
            "opId": Uuid::new_v4().to_string(),
            "logicalTsUnixMs": logical_ts_unix_ms,
            "controllerId": rep.controller_id,
            "dcId": rep.dc_id,
            "eventType": event_type,
            "resourceKey": resource_key,
            "body": body,
        });
        let payload = match serde_json::to_vec(&envelope) {
            Ok(b) => b,
            Err(e) => {
                warn!(
                    error = %e,
                    event_type = %event_type,
                    resource_key = %resource_key,
                    "failed to serialize replication envelope"
                );
                return;
            }
        };
        if let Err(e) = self
            .db
            .append_replication_outbox(event_type, resource_key, &payload)
        {
            warn!(
                error = %e,
                event_type = %event_type,
                resource_key = %resource_key,
                "failed to append replication_outbox row"
            );
        }
    }

    /// Open the node's serial console for a VM. Returns the VM name, the
    /// node id, the sender for keystrokes, and the node's output stream.
    async fn connect_vm_console(
        &self,
        vm_key: &str,
        first_data: Vec<u8>,
    ) -> Result<
        (
            String,
            String,
            mpsc::Sender<node_proto::ConsoleMessage>,
            tonic::Streaming<node_proto::ConsoleMessage>,
        ),
        Status,
    > {
        let vm_key = vm_key.trim();
        if vm_key.is_empty() {
            return Err(Status::invalid_argument(
                "vm_name is required on the first console message",
            ));
        }
        let node = self.resolve_node_for_vm(vm_key, "")?;
        let db_vm = self
            .db
            .get_vm(vm_key)
            .map_err(|e| Status::internal(e.to_string()))?
            .or_else(|| {
                self.db
                    .list_vms_for_node(&node.id)
                    .ok()
                    .and_then(|rows| rows.into_iter().find(|vm| vm.name == vm_key))
            })
            .ok_or_else(|| Status::not_found(format!("VM {vm_key} not found")))?;
        let vm_name = db_vm.name.clone();

        let mut admin = self.ensure_admin_client_for_node(&node).await?;

        let (to_node_tx, to_node_rx) = mpsc::channel::<node_proto::ConsoleMessage>(64);
        // Opening message must be queued before the RPC await — the node
        // handler blocks on the first stream message before returning.
        to_node_tx
            .send(node_proto::ConsoleMessage {
                vm_name: vm_name.clone(),
                data: first_data,
            })
            .await
            .map_err(|_| Status::unavailable("failed to open console stream to node"))?;

        let from_node = admin
            .attach_vm_console(ReceiverStream::new(to_node_rx))
            .await
            .map_err(|e| Status::unavailable(format!("node AttachVmConsole: {e}")))?
            .into_inner();
        Ok((vm_name, node.id, to_node_tx, from_node))
    }

    pub(crate) fn audit_actor<T>(request: &Request<T>) -> String {
        if let Some(actor) = AUDIT_ACTOR_OVERRIDE.with(|c| c.borrow().clone()) {
            return actor;
        }
        if let Some(web) = auth::web_identity(request) {
            return match web.kind {
                auth::WebTokenKind::Api => format!("api-token:{}/{}", web.operator, web.token_id),
                auth::WebTokenKind::Session | auth::WebTokenKind::Pending => {
                    format!("web:{}", web.operator)
                }
            };
        }
        auth::peer_cn(request).unwrap_or_else(|| "insecure".to_string())
    }

    fn push_audit_actor(actor: &str) -> AuditActorGuard {
        AUDIT_ACTOR_OVERRIDE.with(|c| {
            *c.borrow_mut() = Some(actor.to_string());
        });
        AuditActorGuard
    }

    pub(crate) fn record_audit(
        &self,
        actor: &str,
        action: &str,
        resource: &str,
        detail: impl Into<String>,
    ) {
        let detail = detail.into();
        if let Err(e) = self.db.append_audit_event(actor, action, resource, &detail) {
            error!(
                error = %e,
                actor = %actor,
                action = %action,
                resource = %resource,
                "failed to append audit_events row"
            );
        }
    }

    pub(crate) fn log_replication_event_required(
        &self,
        actor: &str,
        audit_action: Option<&str>,
        event_type: &str,
        resource_key: &str,
        body: serde_json::Value,
    ) -> Result<(), Status> {
        if let Some(action) = audit_action {
            self.record_audit(actor, action, resource_key, "");
        }
        let Some(rep) = &self.replication else {
            return Ok(());
        };
        let logical_ts_unix_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as i64)
            .unwrap_or(0);
        let envelope = serde_json::json!({
            "schemaVersion": 1,
            "opId": Uuid::new_v4().to_string(),
            "logicalTsUnixMs": logical_ts_unix_ms,
            "controllerId": rep.controller_id,
            "dcId": rep.dc_id,
            "eventType": event_type,
            "resourceKey": resource_key,
            "body": body,
        });
        let payload = serde_json::to_vec(&envelope)
            .map_err(|e| Status::internal(format!("serialize replication envelope: {e}")))?;
        self.db
            .append_replication_outbox(event_type, resource_key, &payload)
            .map_err(|e| Status::internal(format!("append replication outbox row: {e}")))?;
        Ok(())
    }

    // -- DiskLayout handlers ------------------------------------------------

    async fn create_disk_layout_impl(
        &self,
        actor: &str,
        req: controller_proto::CreateDiskLayoutRequest,
    ) -> Result<Response<controller_proto::CreateDiskLayoutResponse>, Status> {
        let incoming = req
            .disk_layout
            .ok_or_else(|| Status::invalid_argument("disk_layout is required"))?;
        let name = validate_network_name(&incoming.name)?;
        let node_id = incoming.node_id.trim().to_string();
        if node_id.is_empty() {
            return Err(Status::invalid_argument("disk_layout.node_id is required"));
        }
        // Node must exist; DiskLayouts target exactly one registered node.
        if self
            .db
            .get_node(&node_id)
            .map_err(|e| Status::internal(e.to_string()))?
            .is_none()
        {
            return Err(Status::not_found(format!(
                "node '{node_id}' is not registered"
            )));
        }
        let layout_nix = incoming.layout_nix.trim().to_string();
        if layout_nix.is_empty() {
            return Err(Status::invalid_argument(
                "disk_layout.layout_nix cannot be empty",
            ));
        }
        // Minimal structural check: the DiskLayout still lowers to disko.
        if !layout_nix.contains("disko.devices") {
            return Err(Status::invalid_argument(
                "disk_layout.layout_nix must define disko.devices",
            ));
        }

        let existing = self
            .db
            .get_disk_layout(&name)
            .map_err(|e| Status::internal(e.to_string()))?;

        let incoming_evacuate = incoming.evacuate;
        let (action, changed_fields, generation) = if let Some(existing) = existing.as_ref() {
            if existing.node_id != node_id {
                return Err(Status::invalid_argument(format!(
                    "cannot change immutable field(s) on disk layout '{name}': node_id \
                     (delete the disk layout and recreate)"
                )));
            }
            if existing.layout_nix == layout_nix && existing.evacuate == incoming_evacuate {
                (
                    controller_proto::ApplyAction::Unchanged as i32,
                    Vec::<String>::new(),
                    existing.generation,
                )
            } else {
                let mut changed = Vec::new();
                if existing.layout_nix != layout_nix {
                    changed.push("layout_nix".to_string());
                }
                if existing.evacuate != incoming_evacuate {
                    changed.push("evacuate".to_string());
                }
                (
                    controller_proto::ApplyAction::Updated as i32,
                    changed,
                    existing.generation.saturating_add(1),
                )
            }
        } else {
            (
                controller_proto::ApplyAction::Created as i32,
                Vec::<String>::new(),
                1,
            )
        };

        if action == controller_proto::ApplyAction::Unchanged as i32 {
            let existing = existing.expect("unchanged implies existing");
            let status = self
                .db
                .get_disk_layout_status(&name)
                .map_err(|e| Status::internal(e.to_string()))?;
            let proto_layout = disk_layout_to_proto(&existing);
            let _ = status; // status not returned on Create; Get exposes it
            return Ok(Response::new(controller_proto::CreateDiskLayoutResponse {
                success: true,
                disk_layout: Some(proto_layout),
                action,
                changed_fields,
            }));
        }

        let row = DiskLayoutRow {
            name: name.clone(),
            node_id: node_id.clone(),
            generation,
            layout_nix: layout_nix.clone(),
            evacuate: incoming_evacuate,
            created_at: String::new(),
            updated_at: String::new(),
        };
        let stored = self
            .db
            .upsert_disk_layout(&row)
            .map_err(|e| Status::internal(format!("upserting disk layout: {e}")))?;

        // Reset status to Pending on any content change so the reconciler
        // picks it up and the node-agent re-observes the new generation.
        self.db
            .upsert_disk_layout_status(&DiskLayoutStatusRow {
                name: name.clone(),
                observed_generation: 0,
                phase: "pending".to_string(),
                refusal_reason: String::new(),
                message: String::new(),
                last_transition_at: String::new(),
            })
            .map_err(|e| Status::internal(format!("resetting disk layout status: {e}")))?;

        self.log_replication_event(
            actor,
            Some("CreateDiskLayout"),
            EVT_DISK_LAYOUT_CREATE,
            &format!("disk-layout/{name}"),
            serde_json::json!({
                "name": name,
                "nodeId": node_id,
                "generation": generation,
                "layoutNix": layout_nix,
                "evacuate": incoming_evacuate,
                "action": match action {
                    x if x == controller_proto::ApplyAction::Created as i32 => "created",
                    x if x == controller_proto::ApplyAction::Updated as i32 => "updated",
                    _ => "unchanged",
                },
                "changedFields": changed_fields,
            }),
        );

        Ok(Response::new(controller_proto::CreateDiskLayoutResponse {
            success: true,
            disk_layout: Some(disk_layout_to_proto(&stored)),
            action,
            changed_fields,
        }))
    }

    async fn get_disk_layout_impl(
        &self,
        req: controller_proto::GetDiskLayoutRequest,
    ) -> Result<Response<controller_proto::GetDiskLayoutResponse>, Status> {
        let name = req.name.trim();
        if name.is_empty() {
            return Err(Status::invalid_argument("name is required"));
        }
        let row = self
            .db
            .get_disk_layout(name)
            .map_err(|e| Status::internal(e.to_string()))?
            .ok_or_else(|| Status::not_found(format!("disk layout '{name}' not found")))?;
        let status = self
            .db
            .get_disk_layout_status(name)
            .map_err(|e| Status::internal(e.to_string()))?;
        Ok(Response::new(controller_proto::GetDiskLayoutResponse {
            disk_layout: Some(disk_layout_to_proto(&row)),
            status: status.map(|s| disk_layout_status_to_proto(&s)),
        }))
    }

    async fn list_disk_layouts_impl(
        &self,
        req: controller_proto::ListDiskLayoutsRequest,
    ) -> Result<Response<controller_proto::ListDiskLayoutsResponse>, Status> {
        let filter = req.node_id.trim();
        let rows = self
            .db
            .list_disk_layouts(if filter.is_empty() {
                None
            } else {
                Some(filter)
            })
            .map_err(|e| Status::internal(e.to_string()))?;
        let statuses = self
            .db
            .list_disk_layout_statuses()
            .map_err(|e| Status::internal(e.to_string()))?;
        let status_by_name: HashMap<String, DiskLayoutStatusRow> =
            statuses.into_iter().map(|s| (s.name.clone(), s)).collect();
        let summaries = rows
            .iter()
            .map(|r| controller_proto::DiskLayoutSummary {
                disk_layout: Some(disk_layout_to_proto(r)),
                status: status_by_name.get(&r.name).map(disk_layout_status_to_proto),
            })
            .collect();
        Ok(Response::new(controller_proto::ListDiskLayoutsResponse {
            disk_layouts: summaries,
        }))
    }

    async fn delete_disk_layout_impl(
        &self,
        actor: &str,
        req: controller_proto::DeleteDiskLayoutRequest,
    ) -> Result<Response<controller_proto::DeleteDiskLayoutResponse>, Status> {
        let name = req.name.trim().to_string();
        if name.is_empty() {
            return Err(Status::invalid_argument("name is required"));
        }
        let existed = self
            .db
            .delete_disk_layout(&name)
            .map_err(|e| Status::internal(format!("deleting disk layout: {e}")))?;
        if existed {
            self.log_replication_event(
                actor,
                Some("DeleteDiskLayout"),
                EVT_DISK_LAYOUT_DELETE,
                &format!("disk-layout/{name}"),
                serde_json::json!({"name": name}),
            );
        }
        Ok(Response::new(controller_proto::DeleteDiskLayoutResponse {
            success: existed,
        }))
    }

    async fn classify_disk_layout_impl(
        &self,
        req: controller_proto::ClassifyDiskLayoutRequest,
    ) -> Result<Response<controller_proto::ClassifyDiskLayoutResponse>, Status> {
        // Controller-side pre-flight: cheap structural checks + target-device
        // extraction. The authoritative verdict still comes from the
        // node-agent classifier, which uses live lsblk state.
        let layout = req
            .disk_layout
            .ok_or_else(|| Status::invalid_argument("disk_layout is required"))?;
        let layout_nix = layout.layout_nix.trim();
        if layout_nix.is_empty() {
            return Err(Status::invalid_argument(
                "disk_layout.layout_nix cannot be empty",
            ));
        }
        if !layout_nix.contains("disko.devices") {
            return Ok(Response::new(
                controller_proto::ClassifyDiskLayoutResponse {
                    safe: false,
                    refusal_reason: "invalid_layout".to_string(),
                    detail: "layout_nix must define disko.devices".to_string(),
                    target_devices: Vec::new(),
                },
            ));
        }
        let target_devices = kcore_disko_types::extract_target_devices(layout_nix);
        if target_devices.is_empty() {
            return Ok(Response::new(
                controller_proto::ClassifyDiskLayoutResponse {
                    safe: false,
                    refusal_reason: kcore_disko_types::refusal::NO_TARGET_DEVICES.to_string(),
                    detail: "layout did not declare any /dev/* target devices".to_string(),
                    target_devices,
                },
            ));
        }
        // Controller does not yet maintain a replicated block-device inventory
        // or a volume->device mapping table. Once those land (Phase 2 follow-up
        // work) we will build an `LsblkSnapshot` here and call
        // `kcore_disko_types::classify_disk_layout`. Until then, the
        // controller's role is strictly structural: validate the manifest,
        // surface the target devices for the operator to review, and let the
        // node-agent have the last word.
        Ok(Response::new(
            controller_proto::ClassifyDiskLayoutResponse {
                safe: true,
                refusal_reason: String::new(),
                detail: "controller pre-flight accepted; authoritative check runs on the node"
                    .to_string(),
                target_devices,
            },
        ))
    }
}

fn disk_layout_to_proto(row: &DiskLayoutRow) -> controller_proto::DiskLayout {
    controller_proto::DiskLayout {
        name: row.name.clone(),
        node_id: row.node_id.clone(),
        generation: row.generation,
        layout_nix: row.layout_nix.clone(),
        created_at: parse_datetime_to_timestamp(&row.created_at),
        updated_at: parse_datetime_to_timestamp(&row.updated_at),
        evacuate: row.evacuate,
    }
}

fn disk_layout_status_to_proto(row: &DiskLayoutStatusRow) -> controller_proto::DiskLayoutStatus {
    let phase = match row.phase.as_str() {
        "pending" => controller_proto::DiskLayoutPhase::Pending as i32,
        "applied" => controller_proto::DiskLayoutPhase::Applied as i32,
        "refused" => controller_proto::DiskLayoutPhase::Refused as i32,
        "failed" => controller_proto::DiskLayoutPhase::Failed as i32,
        _ => controller_proto::DiskLayoutPhase::Unspecified as i32,
    };
    controller_proto::DiskLayoutStatus {
        observed_generation: row.observed_generation,
        phase,
        refusal_reason: row.refusal_reason.clone(),
        message: row.message.clone(),
        last_transition_at: parse_datetime_to_timestamp(&row.last_transition_at),
    }
}

fn shared_filesystem_phase_to_proto(phase: &str) -> i32 {
    match phase {
        "pending" => controller_proto::SharedFilesystemPhase::Pending as i32,
        "bootstrapping" => controller_proto::SharedFilesystemPhase::Bootstrapping as i32,
        "healthy" => controller_proto::SharedFilesystemPhase::Healthy as i32,
        "degraded" => controller_proto::SharedFilesystemPhase::Degraded as i32,
        "failed" => controller_proto::SharedFilesystemPhase::Failed as i32,
        _ => controller_proto::SharedFilesystemPhase::Unspecified as i32,
    }
}

fn shared_filesystem_to_proto(
    row: &SharedFilesystemRow,
    status: Option<SharedFilesystemStatusRow>,
) -> Result<controller_proto::SharedFilesystem, Status> {
    let status = status.map(|s| controller_proto::SharedFilesystemStatus {
        observed_generation: s.observed_generation,
        phase: shared_filesystem_phase_to_proto(&s.phase),
        health_message: s.health_message,
        last_transition_at: parse_datetime_to_timestamp(&s.last_transition_at),
    });
    Ok(controller_proto::SharedFilesystem {
        name: row.name.clone(),
        generation: row.generation,
        spec: Some(
            shared_filesystem_spec::spec_from_json(&row.spec_json)
                .map_err(|e| Status::internal(format!("decode shared filesystem spec: {e}")))?,
        ),
        status,
        created_at: parse_datetime_to_timestamp(&row.created_at),
        updated_at: parse_datetime_to_timestamp(&row.updated_at),
    })
}

fn object_store_phase_to_proto(phase: &str) -> i32 {
    match phase {
        "pending" => controller_proto::ObjectStorePhase::Pending as i32,
        "bootstrapping" => controller_proto::ObjectStorePhase::Bootstrapping as i32,
        "healthy" => controller_proto::ObjectStorePhase::Healthy as i32,
        "degraded" => controller_proto::ObjectStorePhase::Degraded as i32,
        "failed" => controller_proto::ObjectStorePhase::Failed as i32,
        _ => controller_proto::ObjectStorePhase::Unspecified as i32,
    }
}

fn object_store_to_proto(
    row: &ObjectStoreRow,
    status: Option<ObjectStoreStatusRow>,
) -> Result<controller_proto::ObjectStore, Status> {
    let status = status.map(|s| controller_proto::ObjectStoreStatus {
        observed_generation: s.observed_generation,
        phase: object_store_phase_to_proto(&s.phase),
        health_message: s.health_message,
        last_transition_at: parse_datetime_to_timestamp(&s.last_transition_at),
    });
    Ok(controller_proto::ObjectStore {
        name: row.name.clone(),
        generation: row.generation,
        spec: Some(
            object_store_spec::spec_from_json(&row.spec_json)
                .map_err(|e| Status::internal(format!("decode object store spec: {e}")))?,
        ),
        status,
        created_at: parse_datetime_to_timestamp(&row.created_at),
        updated_at: parse_datetime_to_timestamp(&row.updated_at),
    })
}

fn object_user_to_proto(row: &ObjectUserRow) -> controller_proto::ObjectUser {
    controller_proto::ObjectUser {
        name: row.name.clone(),
        spec: Some(controller_proto::ObjectUserSpec {
            store: row.store_name.clone(),
        }),
        access_key: row.access_key.clone(),
        created_at: parse_datetime_to_timestamp(&row.created_at),
    }
}

fn new_object_user_credentials() -> (String, String) {
    let access_key = format!("KC{}", Uuid::new_v4().simple());
    let secret = Uuid::new_v4().to_string().replace('-', "");
    (access_key, secret)
}

fn slurm_host_address(address: &str) -> String {
    match address.rsplit_once(':') {
        Some((host, port)) if !host.contains(':') && port.parse::<u16>().is_ok() => {
            host.to_string()
        }
        _ => address.to_string(),
    }
}

fn slurm_spec_from_proto(
    spec: controller_proto::SlurmClusterSpec,
) -> crate::slurm_cluster_spec::SlurmClusterSpec {
    use crate::slurm_cluster_spec as s;
    s::SlurmClusterSpec {
        cluster_name: spec.cluster_name,
        controller: slurm_controller_from_proto(spec.controller.unwrap_or_default()),
        backup_controller: spec.backup_controller.map(slurm_controller_from_proto),
        login: s::SlurmLoginSpec {
            node_ids: spec.login.map(|l| l.node_ids).unwrap_or_default(),
        },
        accounting: spec
            .accounting
            .map(|a| s::SlurmAccountingSpec {
                enabled: a.enabled,
                node_id: a.node_id,
            })
            .unwrap_or_default(),
        compute: spec
            .compute
            .into_iter()
            .map(|c| s::SlurmComputeSpec {
                node_id: c.node_id,
                address: c.address,
                iface: c.iface,
                partitions: c.partitions,
                reserve: c
                    .reserve
                    .map(|r| s::SlurmReserveSpec {
                        cores: r.cores,
                        memory_mb: r.memory_mb,
                    })
                    .unwrap_or_default(),
                gres: if c.gres.trim().is_empty() {
                    "none".into()
                } else {
                    c.gres
                },
            })
            .collect(),
        partitions: spec
            .partitions
            .into_iter()
            .map(|p| s::SlurmPartitionSpec {
                name: p.name,
                default: p.default,
                max_time: p.max_time,
                state: p.state,
            })
            .collect(),
        users: spec
            .users
            .into_iter()
            .map(|u| s::SlurmUserSpec {
                name: u.name,
                uid: u.uid.max(0) as u32,
                ssh_keys: u.ssh_keys,
            })
            .collect(),
        storage: spec
            .storage
            .map(|st| s::SlurmStorageSpec {
                mode: if st.mode.trim().is_empty() {
                    "nfs".into()
                } else {
                    st.mode
                },
                exports: st.exports,
                filesystem: st.filesystem,
            })
            .unwrap_or_default(),
        extra_config: spec.extra_config,
        allow_controller_host: spec.allow_controller_host,
        gpu_driver: spec.gpu_driver,
    }
}

fn slurm_controller_from_proto(
    controller: controller_proto::SlurmControllerSpec,
) -> crate::slurm_cluster_spec::SlurmControllerSpec {
    crate::slurm_cluster_spec::SlurmControllerSpec {
        node_id: controller.node_id,
        address: controller.address,
        iface: controller.iface,
    }
}

fn slurm_spec_to_proto(
    spec: &crate::slurm_cluster_spec::SlurmClusterSpec,
) -> controller_proto::SlurmClusterSpec {
    controller_proto::SlurmClusterSpec {
        cluster_name: spec.cluster_name.clone(),
        controller: Some(controller_proto::SlurmControllerSpec {
            node_id: spec.controller.node_id.clone(),
            address: spec.controller.address.clone(),
            iface: spec.controller.iface.clone(),
        }),
        backup_controller: spec.backup_controller.as_ref().map(|b| {
            controller_proto::SlurmControllerSpec {
                node_id: b.node_id.clone(),
                address: b.address.clone(),
                iface: b.iface.clone(),
            }
        }),
        login: Some(controller_proto::SlurmLoginSpec {
            node_ids: spec.login.node_ids.clone(),
        }),
        accounting: Some(controller_proto::SlurmAccountingSpec {
            enabled: spec.accounting.enabled,
            node_id: spec.accounting.node_id.clone(),
        }),
        compute: spec
            .compute
            .iter()
            .map(|c| controller_proto::SlurmComputeSpec {
                node_id: c.node_id.clone(),
                address: c.address.clone(),
                iface: c.iface.clone(),
                partitions: c.partitions.clone(),
                reserve: Some(controller_proto::SlurmReserveSpec {
                    cores: c.reserve.cores,
                    memory_mb: c.reserve.memory_mb,
                }),
                gres: c.gres.clone(),
            })
            .collect(),
        partitions: spec
            .partitions
            .iter()
            .map(|p| controller_proto::SlurmPartitionSpec {
                name: p.name.clone(),
                default: p.default,
                max_time: p.max_time.clone(),
                state: p.state.clone(),
            })
            .collect(),
        users: spec
            .users
            .iter()
            .map(|u| controller_proto::SlurmUserSpec {
                name: u.name.clone(),
                uid: u.uid as i32,
                ssh_keys: u.ssh_keys.clone(),
            })
            .collect(),
        storage: Some(controller_proto::SlurmStorageSpec {
            mode: spec.storage.mode.clone(),
            exports: spec.storage.exports.clone(),
            filesystem: spec.storage.filesystem.clone(),
        }),
        extra_config: spec.extra_config.clone(),
        allow_controller_host: spec.allow_controller_host,
        gpu_driver: spec.gpu_driver,
    }
}

fn slurm_random_bytes(len: usize) -> Result<Vec<u8>, Status> {
    use aws_lc_rs::rand::{SecureRandom, SystemRandom};
    let mut buf = vec![0u8; len];
    SystemRandom::new()
        .fill(&mut buf)
        .map_err(|_| Status::internal("generating Slurm secret"))?;
    Ok(buf)
}

fn slurm_hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn slurm_unhex(raw: &str) -> Vec<u8> {
    let mut out = Vec::with_capacity(raw.len() / 2);
    let bytes = raw.as_bytes();
    let mut i = 0;
    while i + 1 < bytes.len() {
        let hi = (bytes[i] as char).to_digit(16);
        let lo = (bytes[i + 1] as char).to_digit(16);
        if let (Some(hi), Some(lo)) = (hi, lo) {
            out.push((hi * 16 + lo) as u8);
        }
        i += 2;
    }
    out
}

fn slurm_cluster_to_proto(
    row: &SlurmClusterRow,
    nodes: &[SlurmClusterNodeRow],
) -> Result<controller_proto::SlurmCluster, Status> {
    let spec = crate::slurm_cluster_spec::spec_from_json(&row.spec_json)
        .map_err(|e| Status::internal(format!("decode slurm spec: {e}")))?;
    let status: serde_json::Value =
        serde_json::from_str(&row.status_json).unwrap_or_else(|_| serde_json::json!({}));
    let phase = status
        .get("phase")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("pending")
        .to_string();
    let message = status
        .get("message")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("")
        .to_string();
    let observed_generation = status
        .get("observedGeneration")
        .and_then(serde_json::Value::as_i64)
        .unwrap_or(0);
    let node_statuses = nodes
        .iter()
        .map(|n| controller_proto::SlurmNodeStatus {
            node_id: n.node_id.clone(),
            roles: n
                .roles
                .split(',')
                .filter(|r| !r.is_empty())
                .map(ToString::to_string)
                .collect(),
            slurm_state: n.slurm_state.clone(),
            applied_generation: n.applied_generation,
            config_hash: n.config_hash.clone(),
        })
        .collect();
    Ok(controller_proto::SlurmCluster {
        name: row.name.clone(),
        generation: row.generation,
        spec: Some(slurm_spec_to_proto(&spec)),
        status: Some(controller_proto::SlurmClusterStatus {
            observed_generation,
            phase,
            message,
            nodes: node_statuses,
        }),
        created_at: parse_datetime_to_timestamp(&row.created_at),
        updated_at: parse_datetime_to_timestamp(&row.updated_at),
    })
}

fn ceph_cluster_to_proto(
    row: &CephClusterRow,
    status: Option<CephClusterStatusRow>,
) -> Result<controller_proto::CephCluster, Status> {
    let status = status.map(|s| controller_proto::CephClusterStatus {
        observed_generation: s.observed_generation,
        phase: match s.phase.as_str() {
            "pending" => controller_proto::CephClusterPhase::Pending as i32,
            "bootstrapping" => controller_proto::CephClusterPhase::Bootstrapping as i32,
            "healthy" => controller_proto::CephClusterPhase::Healthy as i32,
            "degraded" => controller_proto::CephClusterPhase::Degraded as i32,
            "failed" => controller_proto::CephClusterPhase::Failed as i32,
            _ => controller_proto::CephClusterPhase::Unspecified as i32,
        },
        health_message: s.health_message,
        ceph_status_json: s.ceph_status_json,
        last_transition_at: parse_datetime_to_timestamp(&s.last_transition_at),
    });
    Ok(controller_proto::CephCluster {
        name: row.name.clone(),
        generation: row.generation,
        spec: Some(
            ceph_cluster_spec::spec_from_json(&row.spec_json)
                .map_err(|e| Status::internal(format!("decode ceph spec: {e}")))?,
        ),
        status,
        created_at: parse_datetime_to_timestamp(&row.created_at),
        updated_at: parse_datetime_to_timestamp(&row.updated_at),
    })
}

fn cluster_update_phase_to_proto(s: &str) -> i32 {
    match s {
        "pending" => controller_proto::ClusterUpdatePhase::Pending as i32,
        "ready" => controller_proto::ClusterUpdatePhase::Ready as i32,
        "rolling_out" => controller_proto::ClusterUpdatePhase::RollingOut as i32,
        "succeeded" => controller_proto::ClusterUpdatePhase::Succeeded as i32,
        "failed" => controller_proto::ClusterUpdatePhase::Failed as i32,
        "cancelled" => controller_proto::ClusterUpdatePhase::Cancelled as i32,
        "rolling_back" => controller_proto::ClusterUpdatePhase::RollingBack as i32,
        _ => controller_proto::ClusterUpdatePhase::Unspecified as i32,
    }
}

fn cluster_update_approval_to_proto(s: &str) -> i32 {
    match s {
        "awaiting" => {
            controller_proto::ClusterUpdateApprovalStatus::ClusterUpdateApprovalAwaiting as i32
        }
        "approved" => {
            controller_proto::ClusterUpdateApprovalStatus::ClusterUpdateApprovalApproved as i32
        }
        _ => controller_proto::ClusterUpdateApprovalStatus::Unspecified as i32,
    }
}

fn node_update_phase_to_proto(s: &str) -> i32 {
    match s {
        "pending" => controller_proto::NodeUpdatePhase::Pending as i32,
        "prepared" => controller_proto::NodeUpdatePhase::Prepared as i32,
        "succeeded" => controller_proto::NodeUpdatePhase::Succeeded as i32,
        "failed" => controller_proto::NodeUpdatePhase::Failed as i32,
        "cancelled" => controller_proto::NodeUpdatePhase::Cancelled as i32,
        "rolling_back" => controller_proto::NodeUpdatePhase::RollingBack as i32,
        "reboot_required" => controller_proto::NodeUpdatePhase::RebootRequired as i32,
        _ => controller_proto::NodeUpdatePhase::Unspecified as i32,
    }
}

fn host_status_fields(
    db: &crate::db::Database,
    node_id: &str,
) -> (String, String, String, Vec<String>, i32, String) {
    match db.get_node_host_status(node_id) {
        Ok(Some(row)) => (
            row.kcore_version,
            row.booted_generation,
            row.nixos_generation,
            row.update_capabilities
                .split(',')
                .filter(|part| !part.is_empty())
                .map(|part| part.to_string())
                .collect(),
            row.schema_version,
            row.nixpkgs_rev,
        ),
        _ => (
            String::new(),
            String::new(),
            String::new(),
            Vec::new(),
            0,
            String::new(),
        ),
    }
}

fn cluster_update_row_to_proto(
    row: &ClusterUpdateRow,
    spec: controller_proto::ClusterUpdateSpec,
) -> controller_proto::ClusterUpdate {
    controller_proto::ClusterUpdate {
        spec: Some(spec),
        generation: row.generation,
        phase: cluster_update_phase_to_proto(&row.phase),
        approval_status: cluster_update_approval_to_proto(&row.approval_status),
        created_at: parse_datetime_to_timestamp(&row.created_at),
        updated_at: parse_datetime_to_timestamp(&row.updated_at),
    }
}

fn node_update_row_to_proto(row: &ClusterUpdateNodeRow) -> controller_proto::NodeUpdateStatus {
    controller_proto::NodeUpdateStatus {
        update_name: row.update_name.clone(),
        node_id: row.node_id.clone(),
        observed_generation: row.observed_generation,
        phase: node_update_phase_to_proto(&row.phase),
        current_version: row.current_version.clone(),
        target_version: row.target_version.clone(),
        prepared_closure: row.prepared_closure.clone(),
        current_generation: row.current_generation.clone(),
        target_generation: row.target_generation.clone(),
        requires_reboot: row.requires_reboot,
        last_error: row.last_error.clone(),
        last_transition_at: parse_datetime_to_timestamp(&row.last_transition_at),
    }
}

fn postgresql_info(row: &PostgresqlRow) -> controller_proto::PostgresqlInfo {
    controller_proto::PostgresqlInfo {
        name: row.name.clone(),
        database: row.database_name.clone(),
        package: row.package.clone(),
        port: row.port,
        node_id: row.node_id.clone(),
    }
}

fn postgresql_replication_body(row: &PostgresqlRow) -> serde_json::Value {
    serde_json::json!({
        "name": row.name,
        "database": row.database_name,
        "package": row.package,
        "port": row.port,
        "nodeId": row.node_id,
    })
}

impl ControllerService {
    /// Move every VM off `node_id` using the same live-then-cold paths as
    /// [`Controller::drain_node`]. Used by DiskLayout evacuate reconciliation.
    pub async fn drain_node_vms(
        &self,
        node_id: &str,
        target_node: &str,
        options: NodeDrainOptions,
    ) -> Result<NodeDrainOutcome, Status> {
        let source_node = self
            .db
            .get_node(node_id)
            .map_err(|e| Status::internal(e.to_string()))?
            .ok_or_else(|| Status::not_found(format!("node '{node_id}' not found")))?;

        if options.mark_draining_at_start {
            self.db
                .update_node_status(node_id, "draining")
                .map_err(|e| Status::internal(format!("updating node status: {e}")))?;
        }

        let vms = self
            .db
            .list_vms_for_node(node_id)
            .map_err(|e| Status::internal(format!("listing vms: {e}")))?;

        if vms.is_empty() {
            if options.update_final_node_status {
                self.db
                    .update_node_status(node_id, "drained")
                    .map_err(|e| Status::internal(format!("updating node status: {e}")))?;
            }
            return Ok(NodeDrainOutcome {
                migrated: 0,
                errors: Vec::new(),
            });
        }

        let all_nodes = self
            .db
            .list_nodes()
            .map_err(|e| Status::internal(e.to_string()))?;

        let mut migrated = 0i32;
        let mut errors = Vec::new();
        let eligible_nodes: Vec<NodeRow> = all_nodes
            .iter()
            .filter(|n| n.id != node_id)
            .cloned()
            .collect();

        let mut destination_node_ids: std::collections::HashSet<String> =
            std::collections::HashSet::new();

        for vm in &vms {
            if !vm.pci_devices.is_empty()
                || self
                    .vm_gpu_names(&vm.id)
                    .ok()
                    .is_some_and(|names| !names.is_empty())
            {
                if crate::local_migrate::is_local_block(&vm.storage_backend) {
                    errors.push(format!(
                        "VM '{}' uses a GPU or PCI device with {} storage and stays on {}",
                        vm.name, vm.storage_backend, source_node.id
                    ));
                    continue;
                }
                let mut gpu_targets: Vec<NodeRow> = eligible_nodes
                    .iter()
                    .filter(|n| {
                        target_node.is_empty() || n.id == target_node || n.address == target_node
                    })
                    .filter(|n| self.node_supports_backend(n, &vm.storage_backend))
                    .filter(|n| accepts_migrated_vms(n).is_ok())
                    .cloned()
                    .collect();
                if vm.storage_backend == "ceph" {
                    if let Ok(healthy) = self.healthy_ceph_members(&gpu_targets) {
                        gpu_targets = healthy;
                    }
                }
                let mut moved_gpu = false;
                let mut gpu_error = String::new();
                for target in &gpu_targets {
                    let previous_pci = vm.pci_devices.clone();
                    let previous_gpu = self.vm_gpu_names(&vm.id).unwrap_or_default().join(",");
                    if let Err(e) = self.retarget_vm_gpus(vm, target, &[], &[]) {
                        gpu_error = e.message().to_string();
                        continue;
                    }
                    if let Err(e) = self.cold_release_ceph_vm(vm, &source_node).await {
                        self.restore_vm_gpu(&vm.id, &previous_pci, &previous_gpu);
                        gpu_error = e.message().to_string();
                        continue;
                    }
                    if let Err(e) = self.reassign_vm_node(vm, &target.id) {
                        self.restore_vm_gpu(&vm.id, &previous_pci, &previous_gpu);
                        gpu_error = e.message().to_string();
                        continue;
                    }
                    migrated += 1;
                    destination_node_ids.insert(target.id.clone());
                    moved_gpu = true;
                    break;
                }
                if !moved_gpu {
                    if gpu_error.is_empty() {
                        gpu_error = "no node with a free compatible GPU".into();
                    }
                    errors.push(format!("VM '{}': {gpu_error}", vm.name));
                }
                continue;
            }
            let mut backend_eligible: Vec<NodeRow> = eligible_nodes
                .iter()
                .filter(|n| self.node_supports_backend(n, &vm.storage_backend))
                .filter(|n| accepts_migrated_vms(n).is_ok())
                .cloned()
                .collect();
            if vm.storage_backend == "ceph" {
                match self.healthy_ceph_members(&backend_eligible) {
                    Ok(healthy) => backend_eligible = healthy,
                    Err(e) => {
                        errors.push(format!("VM '{}': {e}", vm.name));
                        continue;
                    }
                }
            }
            let target = if !target_node.is_empty() {
                match backend_eligible
                    .iter()
                    .find(|n| n.id == target_node || n.address == target_node)
                {
                    Some(n) => n.clone(),
                    None => {
                        errors.push(format!(
                            "target node '{target_node}' cannot take VM '{}' (storage backend '{}')",
                            vm.name, vm.storage_backend
                        ));
                        continue;
                    }
                }
            } else {
                match self.select_scheduled_node(
                    &backend_eligible,
                    vm.cpu,
                    vm.memory_bytes,
                    "",
                    &[],
                    &self.db.get_vm_anti_affinity(&vm.id).unwrap_or_default(),
                ) {
                    Ok(Some(n)) => n,
                    Ok(None) => {
                        errors.push(format!(
                            "no node with capacity and compatible storage for VM '{}' ({})",
                            vm.name, vm.storage_backend
                        ));
                        continue;
                    }
                    Err(err) => {
                        errors.push(format!("VM '{}': {err}", vm.name));
                        continue;
                    }
                }
            };

            if vm.storage_backend == "ceph" {
                if let Some(volume) = self
                    .db
                    .get_volume_by_vm(&vm.id)
                    .map_err(|e| Status::internal(e.to_string()))
                    .ok()
                    .flatten()
                {
                    let runtime_name = sanitize_nix_attr_key(&vm.name);
                    let dest_host = grpc_address_host(&target.address);
                    match self
                        .live_migrate_vm(
                            vm,
                            &source_node,
                            &target,
                            Some(&volume),
                            &runtime_name,
                            &dest_host,
                        )
                        .await
                    {
                        Ok(_) => {
                            migrated += 1;
                            destination_node_ids.insert(target.id.clone());
                            continue;
                        }
                        Err(err) if err.send_succeeded => {
                            errors.push(format!(
                                "VM '{}' left mid-migration from {} to {}: {}",
                                vm.name, source_node.id, target.id, err.status
                            ));
                            destination_node_ids.insert(target.id.clone());
                            continue;
                        }
                        Err(err) => {
                            warn!(
                                vm = %vm.name,
                                error = %err.status,
                                "live migrate during drain failed before send; falling back to cold"
                            );
                        }
                    }
                }
            }

            if crate::local_migrate::is_local_block(&vm.storage_backend) {
                match self
                    .migrate_local_disk(vm, &source_node, &target, true)
                    .await
                {
                    Ok(_) => {
                        migrated += 1;
                        destination_node_ids.insert(target.id.clone());
                        continue;
                    }
                    Err(err) if err.send_succeeded => {
                        errors.push(format!(
                            "VM '{}' left mid-migration from {} to {}: {}",
                            vm.name, source_node.id, target.id, err.status
                        ));
                        destination_node_ids.insert(target.id.clone());
                        continue;
                    }
                    Err(err) => {
                        errors.push(format!(
                            "VM '{}' left on {}: {}",
                            vm.name, source_node.id, err.status
                        ));
                        continue;
                    }
                }
            }

            if let Err(e) = self.cold_release_ceph_vm(vm, &source_node).await {
                errors.push(format!("VM '{}' left on {}: {e}", vm.name, source_node.id));
                continue;
            }

            if let Err(e) = self.reassign_vm_node(vm, &target.id) {
                errors.push(format!("VM '{}': {e}", vm.name));
                continue;
            }

            migrated += 1;
            destination_node_ids.insert(target.id.clone());
        }

        if let Err(e) = self.push_config_and_await_apply(&source_node).await {
            warn!(node = %node_id, error = %e, "failed to apply config on drained node");
            errors.push(format!(
                "node {node_id} did not apply its post-drain configuration: {e}",
            ));
        }

        for target_id in &destination_node_ids {
            match self.db.get_node(target_id) {
                Ok(Some(target_node)) => {
                    if let Err(e) = self.push_config_and_await_apply(&target_node).await {
                        warn!(node = %target_id, error = %e, "failed to apply config on target node");
                        errors.push(format!(
                            "target node {target_id} did not apply the migrated VM configuration: {e}"
                        ));
                    }
                }
                Ok(None) => errors.push(format!(
                    "target node {target_id} disappeared before its configuration was applied"
                )),
                Err(e) => errors.push(format!("looking up target node {target_id}: {e}")),
            }
        }

        if options.update_final_node_status {
            let final_status = if errors.is_empty() {
                "drained"
            } else {
                "draining"
            };
            self.db
                .update_node_status(node_id, final_status)
                .map_err(|e| Status::internal(format!("updating node status: {e}")))?;
        }

        Ok(NodeDrainOutcome { migrated, errors })
    }

    /// Addresses under which `node` can appear as a Ceph client: its Ceph
    /// public (mon) address and its cluster-network address from every
    /// CephCluster it belongs to, plus the host part of its gRPC address.
    fn ceph_client_hosts_for_node(&self, node: &NodeRow) -> Result<Vec<String>, Status> {
        let mut hosts = vec![grpc_address_host(&node.address)];
        for cluster in self
            .db
            .list_ceph_clusters()
            .map_err(|e| Status::internal(e.to_string()))?
        {
            let Ok(spec) = ceph_cluster_spec::spec_from_json(&cluster.spec_json) else {
                continue;
            };
            for member in spec.nodes.iter().filter(|n| n.node_id == node.id) {
                let addr = member.mon_addr.trim();
                if !addr.is_empty() {
                    hosts.push(grpc_address_host(addr));
                }
                let cluster = grpc_address_host(member.cluster_addr.trim());
                if !cluster.is_empty() {
                    hosts.push(cluster);
                }
            }
        }
        hosts.retain(|h| !h.is_empty());
        hosts.sort();
        hosts.dedup();
        Ok(hosts)
    }

    /// Blocklist the lost node's Ceph clients for every volume of `vm`, via
    /// one of `members` (healthy CephCluster nodes), before the guest is
    /// started anywhere else.
    ///
    /// The images carry no exclusive lock, so without this the rbd-nbd on the
    /// unreachable node keeps writing after the guest restarts elsewhere and
    /// the filesystem is torn apart by two kernels. Any failure here aborts
    /// the failover of this VM: a guest that stays down is recoverable, a
    /// corrupted disk is not. A member that cannot be reached, does not
    /// implement FenceVolume, or reports success without an IP range for the
    /// lost host is skipped. Any other error ends the attempt, because that
    /// answer (image open from the wrong host, blocklist refused) is the
    /// cluster's verdict and not a transport problem.
    async fn fence_vm_volumes(
        &self,
        vm: &VmRow,
        source: &NodeRow,
        members: &[NodeRow],
    ) -> Result<Vec<String>, Status> {
        let volumes = self
            .db
            .list_attached_volumes_for_vm(&vm.id)
            .map_err(|e| Status::internal(format!("listing volumes: {e}")))?;
        if volumes.is_empty() {
            return Err(Status::failed_precondition(format!(
                "VM '{}' has no attached volume rows; cannot tell which RBD images to fence",
                vm.name
            )));
        }
        let hosts = self.ceph_client_hosts_for_node(source)?;
        let mut blocklisted = Vec::new();
        for vol in &volumes {
            let handle = format!("{}/{}", vol.pool, vol.image);
            #[cfg(test)]
            if let Some(hook) = &self.test_fence_hook {
                let added = hook(&handle, &hosts)?;
                if !fence_covers_lost_host(&added) {
                    return Err(Status::failed_precondition(format!(
                        "fence of {handle} did not blocklist an IP range for the lost host"
                    )));
                }
                blocklisted.extend(added);
                continue;
            }
            let mut last_err = Status::unavailable(format!(
                "no healthy Ceph member reachable to fence {handle}"
            ));
            let mut fenced = false;
            for member in members.iter().filter(|m| m.id != source.id) {
                if self.clients.get_storage(&member.address).is_none() {
                    if let Err(e) = self.clients.connect(&member.address).await {
                        last_err = Status::unavailable(format!(
                            "connecting to {} to fence {handle}: {e}",
                            member.id
                        ));
                        continue;
                    }
                }
                let Some(mut storage) = self.clients.get_storage(&member.address) else {
                    continue;
                };
                match storage
                    .fence_volume(node_proto::FenceVolumeRequest {
                        backend_handle: handle.clone(),
                        source_hosts: hosts.clone(),
                    })
                    .await
                {
                    Ok(resp) => {
                        let added = resp.into_inner().blocklisted;
                        if fence_covers_lost_host(&added) {
                            blocklisted.extend(added);
                            fenced = true;
                            break;
                        }
                        // An older agent answers success with an empty list
                        // after the watchers have already dropped. That is
                        // not a fence: the lost host can still map the image.
                        last_err = Status::failed_precondition(format!(
                            "{} reported {handle} fenced without a blocklist range for the lost host",
                            member.id
                        ));
                    }
                    Err(e)
                        if e.code() == tonic::Code::Unavailable
                            || e.code() == tonic::Code::Unimplemented =>
                    {
                        last_err = Status::new(
                            e.code(),
                            format!(
                                "{} did not answer FenceVolume for {handle}: {}",
                                member.id,
                                e.message()
                            ),
                        );
                    }
                    Err(e) => {
                        return Err(Status::new(
                            e.code(),
                            format!("fencing {handle} via {}: {}", member.id, e.message()),
                        ));
                    }
                }
            }
            if !fenced {
                return Err(last_err);
            }
        }
        Ok(blocklisted)
    }

    /// A controller with no replication peers is the only actor. With peers,
    /// only the failover lease holder moves VMs.
    fn failover_may_act(&self) -> bool {
        let Some(replication) = &self.replication else {
            return true;
        };
        let Ok(Some(lease)) = self.db.get_cluster_lease("failover") else {
            return false;
        };
        lease.holder == replication.controller_id && lease.expires_at_ms > crate::ha::now_unix_ms()
    }

    pub fn maintain_failover_lease(&self) {
        let Some(replication) = &self.replication else {
            return;
        };
        let fresh = self
            .db
            .has_fresh_direct_heartbeat(self.failover.node_timeout_secs)
            .unwrap_or(false);
        let current = self.db.get_cluster_lease("failover").ok().flatten();
        let view = current.as_ref().map(|row| crate::ha::LeaseView {
            holder: row.holder.clone(),
            expires_at_ms: row.expires_at_ms,
            term: row.term,
        });
        if let crate::ha::LeaseDecision::Act {
            term,
            expires_at_ms,
        } = crate::ha::decide_lease(
            crate::ha::now_unix_ms(),
            &replication.controller_id,
            true,
            fresh,
            0,
            self.failover.node_timeout_secs.saturating_mul(500),
            self.failover.lease_renew_secs.saturating_mul(1000),
            view.as_ref(),
        ) {
            let claiming = match &current {
                Some(row) => term > row.term || row.holder != replication.controller_id,
                None => true,
            };
            // A new term has to wait until this process has pulled from its
            // peer once. Renewing the term we already hold does not.
            if claiming && !crate::replication::lease_claim_open(crate::ha::now_unix_ms()) {
                return;
            }
            let row = crate::db::ClusterLeaseRow {
                name: "failover".into(),
                holder: replication.controller_id.clone(),
                expires_at_ms,
                term,
            };
            if self.db.upsert_cluster_lease(&row).is_ok() {
                crate::replication::emit_ha_lease(
                    &self.db,
                    &replication.controller_id,
                    &replication.dc_id,
                    &row,
                );
            }
        }
    }

    /// One pass after startup, and only from the controller that already holds
    /// the failover lease. A standby that publishes its stale runtime would
    /// tell the primary that a running guest is stopped.
    fn publish_ha_snapshot_once(&self) {
        use std::sync::atomic::{AtomicU8, Ordering};
        // 0 unseen, 1 started as the lease holder, 2 replica, 3 published.
        static ROLE: AtomicU8 = AtomicU8::new(0);
        let role = ROLE.load(Ordering::Relaxed);
        if role == 2 || role == 3 {
            return;
        }
        let Some(rep) = &self.replication else {
            ROLE.store(3, Ordering::Relaxed);
            return;
        };
        if role == 0 {
            ROLE.store(
                if self.failover_may_act() { 1 } else { 2 },
                Ordering::Relaxed,
            );
        }
        if ROLE.load(Ordering::Relaxed) != 1 || !self.failover_may_act() {
            return;
        }
        let Ok(policies) = self.db.list_vm_ha() else {
            return;
        };
        let Ok(vms) = self.db.list_vms() else {
            return;
        };
        for row in &policies {
            crate::replication::emit_vm_ha(&self.db, &rep.controller_id, &rep.dc_id, row);
        }
        for vm in &vms {
            let heartbeat = policies
                .iter()
                .find(|row| row.vm_id == vm.id)
                .map(|row| row.last_guest_heartbeat)
                .unwrap_or(0);
            crate::replication::emit_vm_state(
                &self.db,
                &rep.controller_id,
                &rep.dc_id,
                &vm.id,
                &vm.runtime_state,
                heartbeat,
            );
        }
        ROLE.store(3, Ordering::Relaxed);
        info!(
            policies = policies.len(),
            vms = vms.len(),
            "published HA policy and runtime to the standby"
        );
    }

    fn replicate_vm_observation(&self, vm_id: &str, runtime_state: &str) {
        let Some(rep) = &self.replication else {
            return;
        };
        let heartbeat = self
            .db
            .get_vm_ha(vm_id)
            .ok()
            .flatten()
            .map(|row| row.last_guest_heartbeat)
            .unwrap_or(0);
        crate::replication::emit_vm_state(
            &self.db,
            &rep.controller_id,
            &rep.dc_id,
            vm_id,
            runtime_state,
            heartbeat,
        );
    }

    pub async fn ha_pass(&self, announced: &mut std::collections::HashSet<String>) {
        self.maintain_failover_lease();
        self.publish_ha_snapshot_once();
        if self.failover.enabled && self.failover_may_act() {
            self.failover_not_ready_nodes(announced).await;
            self.failover_storage_isolated_nodes().await;
            self.replicate_due_zfs().await;
        }
    }

    async fn unfence_returned_node(&self, node_id: &str) -> bool {
        let Ok(Some(node)) = self.db.get_node(node_id) else {
            return false;
        };
        let Ok(hosts) = self.ceph_client_hosts_for_node(&node) else {
            return false;
        };
        if hosts.is_empty() {
            return true;
        }
        let Ok(members) = self.db.list_nodes() else {
            return false;
        };
        for member in members
            .iter()
            .filter(|member| member.id != node_id && member.status == "ready")
        {
            if !self.node_supports_backend(member, "ceph") {
                continue;
            }
            if self.clients.get_storage(&member.address).is_none()
                && self.clients.connect(&member.address).await.is_err()
            {
                continue;
            }
            let Some(mut storage) = self.clients.get_storage(&member.address) else {
                continue;
            };
            if storage
                .unfence_host(node_proto::UnfenceHostRequest {
                    source_hosts: hosts.clone(),
                })
                .await
                .is_ok()
            {
                info!(node_id, "lifted failover blocklist after the node returned");
                return true;
            }
        }
        warn!(
            node_id,
            "could not lift the failover blocklist; the fence hold stays until a Ceph member accepts the removal"
        );
        false
    }

    async fn replicate_due_zfs(&self) {
        let Ok(policies) = self.db.list_vm_ha() else {
            return;
        };
        let now = crate::ha::now_unix_secs();
        for policy in policies {
            if !crate::ha::replica_due(policy.last_replica_at, policy.replica_interval_secs, now) {
                continue;
            }
            let Ok(Some(vm)) = self.db.get_vm(&policy.vm_id) else {
                continue;
            };
            if vm.storage_backend != "zfs" {
                continue;
            }
            let Ok(Some(source)) = self.db.get_node(&vm.node_id) else {
                continue;
            };
            let Ok(Some(dest)) = self.db.get_node(&policy.replica_node) else {
                continue;
            };
            if source.status != "ready" || dest.status != "ready" {
                continue;
            }
            match self
                .stream_zfs_replica(&vm.name, &policy.last_replica_snapshot, &source, &dest)
                .await
            {
                Ok(snapshot) => {
                    let _ = self.db.set_vm_replica_progress(&vm.id, now, &snapshot);
                    if let (Some(rep), Ok(Some(row))) =
                        (&self.replication, self.db.get_vm_ha(&vm.id))
                    {
                        crate::replication::emit_vm_ha(
                            &self.db,
                            &rep.controller_id,
                            &rep.dc_id,
                            &row,
                        );
                    }
                    let _ = self.db.insert_ha_event(
                        &source.id,
                        &vm.id,
                        "replicated",
                        &format!("snapshot={snapshot} replica={}", dest.id),
                    );
                }
                Err(e) => {
                    warn!(vm = %vm.name, error = %e, "zfs replica send failed");
                }
            }
        }
    }

    async fn stream_zfs_replica(
        &self,
        vm_name: &str,
        incremental_from: &str,
        source: &NodeRow,
        dest: &NodeRow,
    ) -> Result<String, Status> {
        if self.clients.get_storage(&source.address).is_none() {
            self.clients
                .connect(&source.address)
                .await
                .map_err(|e| Status::unavailable(format!("connecting to source: {e}")))?;
        }
        if self.clients.get_storage(&dest.address).is_none() {
            self.clients
                .connect(&dest.address)
                .await
                .map_err(|e| Status::unavailable(format!("connecting to replica: {e}")))?;
        }
        let mut pull = self
            .clients
            .get_storage(&source.address)
            .ok_or_else(|| Status::unavailable("source storage client missing"))?;
        let mut recv = self
            .clients
            .get_storage(&dest.address)
            .ok_or_else(|| Status::unavailable("replica storage client missing"))?;
        let response = pull
            .pull_zfs_send(node_proto::PullZfsSendRequest {
                vm_name: vm_name.to_string(),
                incremental_from: incremental_from.to_string(),
                pool: String::new(),
                dataset_prefix: String::new(),
            })
            .await
            .map_err(|e| Status::unavailable(format!("zfs send: {e}")))?;
        let mut inbound = response.into_inner();
        let (tx, rx) = tokio::sync::mpsc::channel(4);
        tokio::spawn(async move {
            while let Ok(Some(chunk)) = inbound.message().await {
                if tx.send(chunk).await.is_err() {
                    break;
                }
            }
        });
        let received = recv
            .recv_zfs_send(tokio_stream::wrappers::ReceiverStream::new(rx))
            .await
            .map_err(|e| Status::unavailable(format!("zfs recv: {e}")))?
            .into_inner();
        if !received.success {
            return Err(Status::internal(received.message));
        }
        Ok(received.snapshot)
    }

    async fn storage_verdict_for_failover(
        &self,
        source: &NodeRow,
        images: &[String],
    ) -> (crate::ha::StorageVerdict, crate::ha::OsdReport) {
        #[cfg(test)]
        {
            let _ = (source, images);
            let verdict = self
                .test_storage_verdict
                .unwrap_or(crate::ha::StorageVerdict::Dead);
            let osd = match verdict {
                crate::ha::StorageVerdict::Alive => crate::ha::OsdReport::Up,
                crate::ha::StorageVerdict::Dead => crate::ha::OsdReport::Down,
                crate::ha::StorageVerdict::Unknown => crate::ha::OsdReport::Unknown,
            };
            (verdict, osd)
        }
        #[cfg(not(test))]
        {
            self.probe_peer_storage(source, images).await
        }
    }

    #[cfg(not(test))]
    async fn probe_peer_storage(
        &self,
        source: &NodeRow,
        images: &[String],
    ) -> (crate::ha::StorageVerdict, crate::ha::OsdReport) {
        let Ok(hosts) = self.ceph_client_hosts_for_node(source) else {
            return (
                crate::ha::StorageVerdict::Unknown,
                crate::ha::OsdReport::Unknown,
            );
        };
        let Ok(members) = self.db.list_nodes() else {
            return (
                crate::ha::StorageVerdict::Unknown,
                crate::ha::OsdReport::Unknown,
            );
        };
        let host_name = if source.hostname.is_empty() {
            source.id.clone()
        } else {
            source.hostname.clone()
        };
        for member in members
            .iter()
            .filter(|member| member.id != source.id && member.status == "ready")
        {
            if !self.node_supports_backend(member, "ceph") {
                continue;
            }
            if self.clients.get_storage(&member.address).is_none()
                && self.clients.connect(&member.address).await.is_err()
            {
                continue;
            }
            let Some(mut storage) = self.clients.get_storage(&member.address) else {
                continue;
            };
            match storage
                .probe_host_storage(node_proto::ProbeHostStorageRequest {
                    host_name: host_name.clone(),
                    source_hosts: hosts.clone(),
                    images: images.to_vec(),
                })
                .await
            {
                Ok(resp) => {
                    let probe = resp.into_inner();
                    if !probe.queried {
                        continue;
                    }
                    let watcher = if images.is_empty() {
                        None
                    } else {
                        Some(probe.watcher_from_host)
                    };
                    let osd = crate::ha::parse_osd_report(&probe.osd_state);
                    return (crate::ha::storage_verdict(osd, watcher), osd);
                }
                Err(err)
                    if err.code() == tonic::Code::Unimplemented
                        || err.code() == tonic::Code::Unavailable =>
                {
                    continue;
                }
                Err(err) => {
                    warn!(
                        node_id = %source.id,
                        member = %member.id,
                        error = %err,
                        "storage heartbeat failed"
                    );
                }
            }
        }
        (
            crate::ha::StorageVerdict::Unknown,
            crate::ha::OsdReport::Unknown,
        )
    }

    /// Move Ceph VMs off a node that has already been marked `not-ready`.
    ///
    /// Local-disk VMs stay, because their disks are on the failed host. The
    /// source is not contacted: it already missed the heartbeat deadline, and
    /// a connect attempt would only delay the move. Its Ceph clients are
    /// fenced through a healthy member first, so whatever still runs there
    /// can no longer write to the images. The destination receives the VM
    /// unit. The source is recorded so its next successful heartbeat pushes a
    /// config that no longer starts the moved guests.
    pub async fn failover_unreachable_node(
        &self,
        node_id: &str,
    ) -> Result<NodeDrainOutcome, Status> {
        self.failover_node(node_id, false).await
    }

    /// Fence and move Ceph guests off a host that is still heartbeating but
    /// whose OSD a peer has seen down, with no watcher left from that host.
    pub async fn failover_storage_isolated_node(
        &self,
        node_id: &str,
    ) -> Result<NodeDrainOutcome, Status> {
        self.failover_node(node_id, true).await
    }

    async fn failover_node(
        &self,
        node_id: &str,
        live_storage: bool,
    ) -> Result<NodeDrainOutcome, Status> {
        let source = self
            .db
            .get_node(node_id)
            .map_err(|e| Status::internal(e.to_string()))?
            .ok_or_else(|| Status::not_found(format!("node '{node_id}' not found")))?;
        let lost = self.db.node_lost_at(node_id).unwrap_or_default();
        if live_storage {
            if source.status == "not-ready" {
                return Ok(NodeDrainOutcome {
                    migrated: 0,
                    errors: Vec::new(),
                });
            }
        } else if source.status != "not-ready" && lost.is_empty() {
            return Ok(NodeDrainOutcome {
                migrated: 0,
                errors: Vec::new(),
            });
        }
        // A fence hold keeps `lost_at` after the host heartbeats again, so the
        // next beat can still find the placement reports. That timestamp must
        // not move the guests a second time. A storage cut is the exception:
        // the host is still answering, and the peer probe is what says the
        // disk is gone.
        let recently_seen = self
            .db
            .node_recently_seen(node_id, self.failover.node_timeout_secs)
            .unwrap_or(false);
        if !live_storage && recently_seen && source.status != "not-ready" {
            return Ok(NodeDrainOutcome {
                migrated: 0,
                errors: Vec::new(),
            });
        }
        if !self.failover_may_act() {
            return Ok(NodeDrainOutcome {
                migrated: 0,
                errors: Vec::new(),
            });
        }

        let mut vms = self
            .db
            .list_vms_for_node(node_id)
            .map_err(|e| Status::internal(format!("listing vms: {e}")))?;
        let policies = self.db.list_vm_ha().unwrap_or_default();
        crate::ha::sort_for_restart(&mut vms, |vm| {
            policies
                .iter()
                .find(|row| row.vm_id == vm.id)
                .map(|row| row.restart_priority.clone())
                .unwrap_or_else(|| crate::ha::PRIORITY_MEDIUM.to_string())
        });
        if live_storage && !vms.iter().any(|vm| vm.storage_backend == "ceph") {
            return Ok(NodeDrainOutcome {
                migrated: 0,
                errors: Vec::new(),
            });
        }
        if vms.iter().any(|vm| vm.storage_backend == "ceph") {
            let mut images = Vec::new();
            for vm in vms.iter().filter(|vm| vm.storage_backend == "ceph") {
                let Ok(volumes) = self.db.list_attached_volumes_for_vm(&vm.id) else {
                    continue;
                };
                for vol in volumes {
                    images.push(format!("{}/{}", vol.pool, vol.image));
                }
            }
            match self.storage_verdict_for_failover(&source, &images).await {
                (crate::ha::StorageVerdict::Alive, _) => {
                    if !live_storage {
                        let _ = self.db.insert_ha_event(
                            node_id,
                            "",
                            "agent-down",
                            "storage heartbeat still sees this host",
                        );
                    }
                    return Ok(NodeDrainOutcome {
                        migrated: 0,
                        errors: Vec::new(),
                    });
                }
                (crate::ha::StorageVerdict::Unknown, _) => {
                    return Ok(NodeDrainOutcome {
                        migrated: 0,
                        errors: if live_storage {
                            Vec::new()
                        } else {
                            vec![format!(
                                "storage heartbeat for {node_id} did not answer; guests stay"
                            )]
                        },
                    });
                }
                (crate::ha::StorageVerdict::Dead, osd) => {
                    if live_storage && !crate::ha::live_storage_should_move(osd, Some(false)) {
                        return Ok(NodeDrainOutcome {
                            migrated: 0,
                            errors: Vec::new(),
                        });
                    }
                }
            }
        }
        let all_nodes = self
            .db
            .list_nodes()
            .map_err(|e| Status::internal(e.to_string()))?;
        let eligible: Vec<NodeRow> = all_nodes
            .iter()
            .filter(|n| n.id != node_id)
            .cloned()
            .collect();

        let mut migrated = 0i32;
        let mut errors = Vec::new();
        for vm in &vms {
            let policy = policies.iter().find(|row| row.vm_id == vm.id);
            let replica_node = policy.map(|row| row.replica_node.as_str()).unwrap_or("");
            let has_replica = vm.storage_backend == "zfs"
                && !replica_node.is_empty()
                && policy.map(|row| row.last_replica_at).unwrap_or(0) > 0;
            if live_storage && vm.storage_backend != "ceph" && !has_replica {
                continue;
            }
            if vm.storage_backend != "ceph" && !has_replica {
                errors.push(format!(
                    "VM '{}' uses {} storage on failed node {node_id}; it stays until the node returns",
                    vm.name, vm.storage_backend
                ));
                let _ = self.db.insert_ha_event(
                    node_id,
                    &vm.id,
                    "skipped",
                    &format!("unprotected {}", vm.storage_backend),
                );
                continue;
            }
            let (target, blocklisted) = if has_replica {
                let Some(node) = eligible.iter().find(|n| n.id == replica_node).cloned() else {
                    errors.push(format!(
                        "VM '{}' replica node {replica_node} is not available",
                        vm.name
                    ));
                    continue;
                };
                if let Err(e) = accepts_migrated_vms(&node) {
                    errors.push(format!("VM '{}': {e}", vm.name));
                    continue;
                }
                (node, Vec::new())
            } else {
                let mut candidates: Vec<NodeRow> = eligible
                    .iter()
                    .filter(|n| self.node_supports_backend(n, "ceph"))
                    .filter(|n| accepts_migrated_vms(n).is_ok())
                    .cloned()
                    .collect();
                match self.healthy_ceph_members(&candidates) {
                    Ok(healthy) => candidates = healthy,
                    Err(e) => {
                        errors.push(format!("VM '{}': {e}", vm.name));
                        continue;
                    }
                }
                let target = match self.select_scheduled_node(
                    &candidates,
                    vm.cpu,
                    vm.memory_bytes,
                    "",
                    &[],
                    &self.db.get_vm_anti_affinity(&vm.id).unwrap_or_default(),
                ) {
                    Ok(Some(node)) => node,
                    Ok(None) => {
                        errors.push(format!(
                            "no healthy Ceph node can take VM '{}' from failed node {node_id}",
                            vm.name
                        ));
                        continue;
                    }
                    Err(err) => {
                        errors.push(format!("VM '{}': {err}", vm.name));
                        continue;
                    }
                };
                let blocklisted = match self.fence_vm_volumes(vm, &source, &candidates).await {
                    Ok(addrs) => addrs,
                    Err(e) => {
                        errors.push(format!(
                            "VM '{}' stays on {node_id}: its RBD images could not be fenced ({})",
                            vm.name,
                            e.message()
                        ));
                        let _ = self
                            .db
                            .insert_ha_event(node_id, &vm.id, "failed", e.message());
                        self.webhooks.emit(
                            crate::webhooks::EVENT_VM_FAILOVER_FAILED,
                            format!("vm/{}", vm.id),
                            serde_json::json!({
                                "vmId": vm.id,
                                "sourceNode": node_id,
                                "error": e.message(),
                            }),
                        );
                        continue;
                    }
                };
                info!(
                    vm = %vm.name,
                    source = %node_id,
                    blocklisted = ?blocklisted,
                    "fenced Ceph clients of unreachable node"
                );
                if let Err(e) = self.db.add_fenced_vm(node_id, &vm.name) {
                    warn!(
                        node_id,
                        vm = %vm.name,
                        error = %e,
                        "fenced the guest but could not record the hold"
                    );
                }
                (target, blocklisted)
            };
            let restart = crate::ha::should_restart(&vm.runtime_state, vm.auto_start);
            if restart != vm.auto_start {
                let _ = self.db.set_vm_auto_start(&vm.id, restart);
            }

            if let Err(e) = self.reassign_vm_node(vm, &target.id) {
                errors.push(format!("VM '{}': {e}", vm.name));
                continue;
            }
            migrated += 1;
            if let Err(e) = self.db.mark_node_config_push(node_id) {
                warn!(node_id, error = %e, "failed to record config refresh for failed node");
            }
            self.webhooks.emit(
                crate::webhooks::EVENT_VM_MIGRATED,
                format!("vm/{}", vm.id),
                serde_json::json!({
                    "vmId": vm.id,
                    "vmName": vm.name,
                    "sourceNode": node_id,
                    "targetNode": target.id,
                    "mode": if has_replica { "replica" } else { "failover" },
                }),
            );
            let _ = self.db.insert_ha_event(
                node_id,
                &vm.id,
                "reassigned",
                &format!("target={} restart={restart}", target.id),
            );
            self.webhooks.emit(
                crate::webhooks::EVENT_VM_FAILOVER_COMPLETED,
                format!("vm/{}", vm.id),
                serde_json::json!({
                    "vmId": vm.id,
                    "sourceNode": node_id,
                    "targetNode": target.id,
                    "restart": restart,
                }),
            );
            let _ = self.log_replication_event_required(
                "controller",
                Some("FailoverVm"),
                EVT_VM_MIGRATE,
                &format!("vm/{}", vm.id),
                serde_json::json!({
                    "vmId": vm.id,
                    "vmName": vm.name,
                    "sourceNode": node_id,
                    "targetNode": target.id,
                    "mode": "failover",
                    "blocklisted": blocklisted,
                }),
            );
            if let Err(e) = self.push_config_and_await_apply(&target).await {
                errors.push(format!(
                    "VM '{}' is assigned to {} but that node did not apply its configuration: {e}",
                    vm.name, target.id
                ));
            }
            info!(
                vm = %vm.name,
                source = %node_id,
                target = %target.id,
                "moved Ceph VM off unreachable node"
            );
        }

        Ok(NodeDrainOutcome { migrated, errors })
    }

    /// Ceph guests on a host that is still heartbeating, but whose OSD a
    /// peer reports down and whose watcher is gone.
    async fn failover_storage_isolated_nodes(&self) {
        let nodes = match self.db.list_nodes() {
            Ok(nodes) => nodes,
            Err(e) => {
                warn!(error = %e, "storage isolation could not list nodes");
                return;
            }
        };
        for node in nodes {
            if node.status != "ready" && node.status != "cordoned" {
                continue;
            }
            let health = self.db.node_host_health(&node.id).unwrap_or_default();
            if !crate::ha::storage_probe_due(&health) {
                continue;
            }
            match self.failover_storage_isolated_node(&node.id).await {
                Ok(outcome) => {
                    if outcome.migrated > 0 {
                        info!(
                            node_id = %node.id,
                            migrated = outcome.migrated,
                            "storage cut moved Ceph VMs"
                        );
                    }
                    for err in outcome.errors {
                        warn!(node_id = %node.id, "{err}");
                    }
                }
                Err(e) => {
                    warn!(node_id = %node.id, error = %e, "storage-cut failover failed");
                }
            }
        }
    }

    /// One pass of automatic failover for every node currently `not-ready`.
    ///
    /// `announced` remembers errors already logged for a node so a VM that
    /// cannot move does not warn on every pass. Entries are dropped once the
    /// node is no longer `not-ready`.
    pub async fn failover_not_ready_nodes(
        &self,
        announced: &mut std::collections::HashSet<String>,
    ) {
        let nodes = match self.db.list_nodes() {
            Ok(nodes) => nodes,
            Err(e) => {
                warn!(error = %e, "failover could not list nodes");
                return;
            }
        };
        let lost_ids = self.db.list_lost_node_ids().unwrap_or_default();
        let not_ready: std::collections::HashSet<String> = nodes
            .iter()
            .filter(|node| node.status == "not-ready" || lost_ids.iter().any(|id| id == &node.id))
            .map(|node| node.id.clone())
            .collect();
        announced.retain(|key| {
            not_ready
                .iter()
                .any(|id| key.starts_with(&format!("{id}/")))
        });
        for node_id in not_ready {
            match self.failover_unreachable_node(&node_id).await {
                Ok(outcome) => {
                    if outcome.migrated > 0 {
                        info!(
                            node_id,
                            migrated = outcome.migrated,
                            "automatic failover moved Ceph VMs"
                        );
                    }
                    for err in outcome.errors {
                        if announced.insert(format!("{node_id}/{err}")) {
                            warn!(node_id, "{err}");
                        }
                    }
                }
                Err(e) => {
                    let err = e.to_string();
                    if announced.insert(format!("{node_id}/{err}")) {
                        warn!(node_id, error = %err, "automatic failover failed");
                    }
                }
            }
        }
    }
    async fn connect_node_storage(
        &self,
        node: &NodeRow,
    ) -> Result<node_proto::node_storage_client::NodeStorageClient<tonic::transport::Channel>, Status>
    {
        if self.clients.get_storage(&node.address).is_none() {
            self.clients
                .connect(&node.address)
                .await
                .map_err(|e| Status::unavailable(format!("connecting to {}: {e}", node.id)))?;
        }
        self.clients.get_storage(&node.address).ok_or_else(|| {
            Status::unavailable(format!("storage client unavailable for {}", node.id))
        })
    }

    fn host_volume_event(&self, vol: &VolumeRow) -> serde_json::Value {
        serde_json::json!({
            "id": vol.id,
            "name": vol.name,
            "role": vol.role,
            "pool": vol.pool,
            "image": vol.image,
            "sizeBytes": vol.size_bytes,
            "vmId": vol.vm_id,
            "attachState": vol.attach_state,
            "serial": vol.serial,
            "slot": vol.slot,
            "storageClass": vol.storage_class,
            "hostNodeId": vol.host_node_id,
            "fsType": vol.fs_type,
            "mountPoint": vol.mount_point,
        })
    }

    async fn create_host_volume(
        &self,
        actor: &str,
        name: &str,
        storage_class: &str,
        req: &controller_proto::CreateVolumeRequest,
    ) -> Result<Response<controller_proto::CreateVolumeResponse>, Status> {
        if req.encrypt {
            return Err(Status::invalid_argument(
                "encrypt is only supported for ceph volumes",
            ));
        }
        if !req.from_snapshot.trim().is_empty() {
            return Err(Status::invalid_argument(
                "from_snapshot is only supported for ceph volumes",
            ));
        }
        if !req.vm.trim().is_empty() {
            return Err(Status::invalid_argument(
                "lvm and zfs volumes are mounted on their node, not attached to a VM",
            ));
        }
        let node_id = req.node.trim();
        if node_id.is_empty() {
            return Err(Status::invalid_argument(
                "node is required for lvm and zfs volumes",
            ));
        }
        if self
            .db
            .get_volume_by_name(name)
            .map_err(|e| Status::internal(e.to_string()))?
            .is_some()
        {
            return Err(Status::already_exists(format!(
                "volume '{name}' already exists"
            )));
        }
        let node = self
            .db
            .get_node(node_id)
            .map_err(|e| Status::internal(e.to_string()))?
            .ok_or_else(|| Status::not_found(format!("node '{node_id}' not found")))?;
        if node.storage_backend != storage_class {
            return Err(Status::failed_precondition(format!(
                "node {} storage backend is {}, not {storage_class}",
                node.id, node.storage_backend
            )));
        }
        let mut row = VolumeRow::new_data(name, req.size_bytes);
        row.storage_class = storage_class.to_string();
        row.host_node_id = node.id.clone();
        let mut storage = self.connect_node_storage(&node).await?;
        let created = storage
            .create_volume(node_proto::CreateVolumeRequest {
                volume_id: name.to_string(),
                storage_class: storage_class.to_string(),
                size_bytes: row.size_bytes,
                parameters: HashMap::new(),
                lv_name: req.volume_name.trim().to_string(),
            })
            .await
            .map_err(|e| Status::internal(format!("creating {storage_class} volume: {e}")))?
            .into_inner();
        let (pool, image) =
            crate::host_volume::split_backend_handle(storage_class, &created.backend_handle)
                .map_err(Status::internal)?;
        row.pool = pool;
        row.image = image;
        self.db
            .upsert_volume(&row)
            .map_err(|e| Status::internal(format!("storing volume: {e}")))?;
        self.log_replication_event(
            actor,
            Some("volume.create"),
            EVT_VOLUME_CREATE,
            &format!("volume/{}", row.name),
            self.host_volume_event(&row),
        );
        Ok(Response::new(controller_proto::CreateVolumeResponse {
            success: true,
            volume: Some(self.volume_info_from_row(&row, None)),
            message: format!("volume '{name}' created on {}", node.id),
        }))
    }

    async fn resize_host_volume(
        &self,
        actor: String,
        mut vol: VolumeRow,
        size_bytes: i64,
        allow_shrink: bool,
    ) -> Result<Response<controller_proto::ResizeVolumeResponse>, Status> {
        if size_bytes <= 0 {
            return Err(Status::invalid_argument("size_bytes must be positive"));
        }
        if size_bytes == vol.size_bytes {
            return Err(Status::invalid_argument(
                "size_bytes equals current provisioned size; no change",
            ));
        }
        if size_bytes < vol.size_bytes && !allow_shrink {
            return Err(Status::invalid_argument(
                "shrink refused; pass allow_shrink=true to shrink an unmounted ext4 volume",
            ));
        }
        if size_bytes < vol.size_bytes && !vol.mount_point.is_empty() {
            return Err(Status::failed_precondition(
                "unmount the volume before shrinking it",
            ));
        }
        let device = crate::host_volume::device_path(&vol.storage_class, &vol.pool, &vol.image)
            .map_err(Status::failed_precondition)?;
        let node = self.host_volume_node(&vol).await?;
        let mut storage = self.connect_node_storage(&node).await?;
        let resized = storage
            .resize_volume(node_proto::ResizeVolumeRequest {
                backend_handle: device,
                size_bytes,
                allow_shrink,
            })
            .await
            .map_err(|e| Status::internal(format!("resizing volume: {e}")))?
            .into_inner();
        vol.size_bytes = if resized.size_bytes > 0 {
            resized.size_bytes
        } else {
            size_bytes
        };
        vol.generation += 1;
        self.db
            .upsert_volume(&vol)
            .map_err(|e| Status::internal(e.to_string()))?;
        self.log_replication_event(
            &actor,
            Some("volume.update"),
            EVT_VOLUME_UPDATE,
            &format!("volume/{}", vol.name),
            self.host_volume_event(&vol),
        );
        let message = if resized.message.is_empty() {
            format!("resized '{}' to {} bytes", vol.name, vol.size_bytes)
        } else {
            resized.message
        };
        Ok(Response::new(controller_proto::ResizeVolumeResponse {
            success: true,
            message,
            volume: Some(self.volume_info_from_row(&vol, None)),
            operation_id: String::new(),
            guest_status_note: String::new(),
        }))
    }

    async fn delete_host_volume(
        &self,
        actor: String,
        row: VolumeRow,
    ) -> Result<Response<controller_proto::DeleteVolumeResponse>, Status> {
        if !row.mount_point.is_empty() {
            return Err(Status::failed_precondition(
                "unmount the volume before deleting it",
            ));
        }
        let device = crate::host_volume::device_path(&row.storage_class, &row.pool, &row.image)
            .map_err(Status::failed_precondition)?;
        let node = self.host_volume_node(&row).await?;
        let mut storage = self.connect_node_storage(&node).await?;
        storage
            .delete_volume(node_proto::DeleteVolumeRequest {
                backend_handle: device,
            })
            .await
            .map_err(|e| Status::internal(format!("deleting volume: {e}")))?;
        self.db
            .delete_volume_by_id(&row.id)
            .map_err(|e| Status::internal(e.to_string()))?;
        self.log_replication_event(
            &actor,
            Some("volume.delete"),
            EVT_VOLUME_DELETE,
            &format!("volume/{}", row.name),
            serde_json::json!({
                "id": row.id,
                "name": row.name,
            }),
        );
        Ok(Response::new(controller_proto::DeleteVolumeResponse {
            success: true,
            message: format!("deleted volume '{}'", row.name),
        }))
    }

    async fn host_volume_node(&self, vol: &VolumeRow) -> Result<NodeRow, Status> {
        if vol.host_node_id.is_empty() {
            return Err(Status::failed_precondition(
                "host volume has no node recorded",
            ));
        }
        self.db
            .get_node(&vol.host_node_id)
            .map_err(|e| Status::internal(e.to_string()))?
            .ok_or_else(|| {
                Status::failed_precondition(format!("node '{}' is missing", vol.host_node_id))
            })
    }
}

impl ControllerService {
    fn slurm_members(
        &self,
        spec: &crate::slurm_cluster_spec::SlurmClusterSpec,
    ) -> Result<std::collections::BTreeMap<String, crate::slurm_cluster_spec::Member>, Status> {
        let mut login_lookup = std::collections::HashMap::new();
        for node_id in &spec.login.node_ids {
            if let Some(node) = self
                .db
                .get_node(node_id)
                .map_err(|e| Status::internal(format!("reading node {node_id}: {e}")))?
            {
                login_lookup.insert(
                    node_id.clone(),
                    (
                        slurm_host_address(&node.address),
                        spec.controller.iface.clone(),
                    ),
                );
            }
        }
        Ok(crate::slurm_cluster_spec::members(spec, |id| {
            login_lookup.get(id).cloned()
        }))
    }

    fn slurm_gpus(
        &self,
    ) -> Result<std::collections::BTreeMap<String, Vec<crate::slurm_render::GpuEntry>>, Status>
    {
        let mut out: std::collections::BTreeMap<String, Vec<crate::slurm_render::GpuEntry>> =
            std::collections::BTreeMap::new();
        for row in self
            .db
            .list_node_gpus(None)
            .map_err(|e| Status::internal(format!("listing GPUs: {e}")))?
        {
            if !row.kind.is_empty() && row.kind != "gpu" {
                continue;
            }
            if !row.parent.is_empty() {
                continue;
            }
            out.entry(row.node_id)
                .or_default()
                .push(crate::slurm_render::GpuEntry {
                    model: if row.model.is_empty() {
                        row.name
                    } else {
                        row.model
                    },
                    address: row.address,
                });
        }
        Ok(out)
    }

    fn slurm_self_identity(&self) -> (Option<String>, Option<String>) {
        let id = self
            .replication
            .as_ref()
            .map(|replication| replication.controller_id.clone());
        let host = std::fs::read_to_string("/etc/hostname")
            .ok()
            .map(|h| h.trim().to_string())
            .filter(|h| !h.is_empty())
            .or_else(|| std::env::var("HOSTNAME").ok());
        (id, host)
    }

    async fn plan_slurm_cluster_spec(
        &self,
        name: &str,
        spec: &crate::slurm_cluster_spec::SlurmClusterSpec,
    ) -> Result<controller_proto::PlanSlurmClusterResponse, Status> {
        let existing_row = self
            .db
            .get_slurm_cluster(name)
            .map_err(|e| Status::internal(format!("reading slurm cluster: {e}")))?;
        let existing_spec = existing_row
            .as_ref()
            .and_then(|row| crate::slurm_cluster_spec::spec_from_json(&row.spec_json).ok());
        let (action, _changed) =
            crate::slurm_cluster_spec::plan_action(existing_spec.as_ref(), spec);
        let mut blocking = Vec::new();
        let mut warnings = Vec::new();
        let mut node_plans = Vec::new();

        if let Err(err) = crate::slurm_cluster_spec::validate_spec(spec) {
            blocking.push(err);
            return Ok(controller_proto::PlanSlurmClusterResponse {
                action: action.to_string(),
                blocking_issues: blocking,
                warnings,
                node_plans,
                rendered_nix_preview: String::new(),
            });
        }
        if action == "replace" {
            let field = existing_spec
                .as_ref()
                .and_then(|old| crate::slurm_cluster_spec::immutable_change(old, spec))
                .unwrap_or_else(|| "spec".into());
            blocking.push(format!(
                "immutable field {field} changed; delete the SlurmCluster and create it again"
            ));
        }

        let members = self.slurm_members(spec)?;
        let gpus = self.slurm_gpus()?;
        let (self_id, self_host) = self.slurm_self_identity();
        for member in members.values() {
            let mut issues = Vec::new();
            let node = self
                .db
                .get_node(&member.node_id)
                .map_err(|e| Status::internal(format!("reading node {}: {e}", member.node_id)))?;
            match node.as_ref() {
                None => issues.push(format!("node '{}' is not registered", member.node_id)),
                Some(node) => {
                    if member.has_role("compute") {
                        let vms = self
                            .db
                            .list_vms_for_node(&member.node_id)
                            .map_err(|e| Status::internal(format!("listing VMs: {e}")))?;
                        if !vms.is_empty() {
                            issues.push(format!(
                                "compute member hosts {} guest VM(s); drain them before apply",
                                vms.len()
                            ));
                        }
                    }
                    if member.has_role("compute")
                        && !spec.allow_controller_host
                        && (self_id.as_deref() == Some(member.node_id.as_str())
                            || self_host.as_deref() == Some(node.hostname.as_str())
                            || self_host.as_deref() == Some(member.node_id.as_str()))
                    {
                        issues.push(format!(
                            "compute node '{}' is the kcore controller host; set allowControllerHost: true to override",
                            member.node_id
                        ));
                    }
                }
            }
            if let Some(other) = self
                .db
                .get_slurm_cluster_node(&member.node_id)
                .map_err(|e| Status::internal(format!("reading slurm membership: {e}")))?
            {
                if other.cluster_name != name {
                    issues.push(format!(
                        "node '{}' already belongs to SlurmCluster '{}'",
                        member.node_id, other.cluster_name
                    ));
                }
            }
            if let Some(compute) = &member.compute {
                if compute.gres == "auto" {
                    if !spec.gpu_driver {
                        issues.push(format!(
                            "compute node '{}' asks for gres: auto but the host NVIDIA driver is off; set gpuDriver: true only when the driver can be built on the node",
                            member.node_id
                        ));
                    } else if gpus
                        .get(&member.node_id)
                        .map(|entries| entries.is_empty())
                        .unwrap_or(true)
                    {
                        issues.push(format!(
                            "compute node '{}' asks for gres: auto but no GPU inventory was reported",
                            member.node_id
                        ));
                    }
                }
            }
            for issue in &issues {
                blocking.push(format!("{}: {issue}", member.node_id));
            }
            node_plans.push(controller_proto::NodePlanResult {
                node_id: member.node_id.clone(),
                preflight_passed: issues.is_empty(),
                issues,
                nix_action: if existing_row.is_some() {
                    "rebuild".into()
                } else {
                    "build".into()
                },
            });
        }

        if spec.storage.mode == "shared-filesystem" {
            match self
                .db
                .get_shared_filesystem(&spec.storage.filesystem)
                .map_err(|e| Status::internal(format!("reading shared filesystem: {e}")))?
            {
                None => blocking.push(format!(
                    "storage.filesystem '{}' does not exist",
                    spec.storage.filesystem
                )),
                Some(_) => blocking.push(
                    "shared-filesystem mode is refused: the SharedFilesystem reconciler cannot issue a CephFS client key yet (see docs/cephfs-rgw.md); use nfs or none"
                        .into(),
                ),
            }
        }
        if spec.storage.mode == "nfs" {
            warnings.push(
                "NFS exports are served by the controller node and are a single point of failure; if it stops, every member loses /home and /scratch"
                    .into(),
            );
        }
        warnings
            .push("compute members are cordoned so the VM scheduler leaves them to slurmd".into());

        let rendered_nix_preview = members
            .values()
            .next()
            .map(|member| {
                crate::slurm_render::render_node_fragment(
                    spec,
                    member,
                    &members,
                    &gpus,
                    spec.gpu_driver && member.has_role("compute"),
                )
            })
            .unwrap_or_default();

        Ok(controller_proto::PlanSlurmClusterResponse {
            action: action.to_string(),
            blocking_issues: blocking,
            warnings,
            node_plans,
            rendered_nix_preview,
        })
    }

    fn slurm_replication_body(
        row: &SlurmClusterRow,
        nodes: &[SlurmClusterNodeRow],
    ) -> serde_json::Value {
        serde_json::json!({
            "name": row.name,
            "generation": row.generation,
            "specJson": row.spec_json,
            "statusJson": row.status_json,
            "mungeKeyHex": slurm_hex(&row.munge_key),
            "dbdPassword": row.dbd_password,
            "nodes": nodes.iter().map(|n| serde_json::json!({
                "nodeId": n.node_id,
                "roles": n.roles,
                "appliedGeneration": n.applied_generation,
                "slurmState": n.slurm_state,
                "configHash": n.config_hash,
            })).collect::<Vec<_>>(),
        })
    }

    async fn slurm_control_rpc(
        &self,
        cluster_name: &str,
        node_id: &str,
        verb: &str,
        reason: &str,
    ) -> Result<String, Status> {
        let row = self
            .db
            .get_slurm_cluster(cluster_name)
            .map_err(|e| Status::internal(format!("reading slurm cluster: {e}")))?
            .ok_or_else(|| Status::not_found(format!("SlurmCluster '{cluster_name}' not found")))?;
        let spec = crate::slurm_cluster_spec::spec_from_json(&row.spec_json)
            .map_err(|e| Status::internal(format!("decode slurm spec: {e}")))?;
        let members = self.slurm_members(&spec)?;
        let member = members
            .get(node_id)
            .ok_or_else(|| Status::not_found(format!("node '{node_id}' is not a member")))?;
        if !member.has_role("compute") {
            return Err(Status::invalid_argument(format!(
                "node '{node_id}' is not a compute member"
            )));
        }
        let controller = self
            .db
            .get_node(&spec.controller.node_id)
            .map_err(|e| Status::internal(format!("reading controller node: {e}")))?
            .ok_or_else(|| Status::not_found("controller node is not registered"))?;
        let mut admin = self.ensure_admin_client_for_node(&controller).await?;
        let resp = admin
            .slurm_control(Request::new(node_proto::SlurmControlRequest {
                verb: verb.to_string(),
                node_name: node_id.to_string(),
                reason: reason.to_string(),
            }))
            .await
            .map_err(|e| Status::unavailable(format!("node SlurmControl: {e}")))?
            .into_inner();
        if !resp.success {
            return Err(Status::failed_precondition(resp.message));
        }
        Ok(resp.message)
    }
}

#[tonic::async_trait]
impl controller_proto::controller_server::Controller for ControllerService {
    async fn format_volume(
        &self,
        request: Request<controller_proto::FormatVolumeRequest>,
    ) -> Result<Response<controller_proto::FormatVolumeResponse>, Status> {
        self.require_operator(&request, OperatorRole::VmAdmin)?;
        let actor = Self::audit_actor(&request);
        let req = request.into_inner();
        let mut vol = self
            .resolve_volume_row(req.name.trim())?
            .ok_or_else(|| Status::not_found(format!("volume '{}' not found", req.name)))?;
        if !crate::host_volume::is_host(&vol.storage_class) {
            return Err(Status::failed_precondition(
                "format is supported for lvm and zfs volumes",
            ));
        }
        if !vol.mount_point.is_empty() {
            return Err(Status::failed_precondition(
                "unmount the volume before formatting it",
            ));
        }
        let fs_type = req.fs_type.trim().to_ascii_lowercase();
        let device = crate::host_volume::device_path(&vol.storage_class, &vol.pool, &vol.image)
            .map_err(Status::failed_precondition)?;
        let node = self.host_volume_node(&vol).await?;
        let mut storage = self.connect_node_storage(&node).await?;
        let formatted = storage
            .format_volume(node_proto::FormatVolumeRequest {
                backend_handle: device,
                fs_type: fs_type.clone(),
                force: req.force,
            })
            .await
            .map_err(|e| Status::internal(format!("formatting volume: {e}")))?
            .into_inner();
        vol.fs_type = fs_type;
        vol.generation += 1;
        self.db
            .upsert_volume(&vol)
            .map_err(|e| Status::internal(e.to_string()))?;
        self.log_replication_event(
            &actor,
            Some("volume.update"),
            EVT_VOLUME_UPDATE,
            &format!("volume/{}", vol.name),
            self.host_volume_event(&vol),
        );
        Ok(Response::new(controller_proto::FormatVolumeResponse {
            success: true,
            message: formatted.message,
            volume: Some(self.volume_info_from_row(&vol, None)),
        }))
    }

    async fn mount_volume(
        &self,
        request: Request<controller_proto::MountVolumeRequest>,
    ) -> Result<Response<controller_proto::MountVolumeResponse>, Status> {
        self.require_operator(&request, OperatorRole::VmAdmin)?;
        let actor = Self::audit_actor(&request);
        let req = request.into_inner();
        let mut vol = self
            .resolve_volume_row(req.name.trim())?
            .ok_or_else(|| Status::not_found(format!("volume '{}' not found", req.name)))?;
        if !crate::host_volume::is_host(&vol.storage_class) {
            return Err(Status::failed_precondition(
                "mount is supported for lvm and zfs volumes",
            ));
        }
        let device = crate::host_volume::device_path(&vol.storage_class, &vol.pool, &vol.image)
            .map_err(Status::failed_precondition)?;
        let node = self.host_volume_node(&vol).await?;
        let mut storage = self.connect_node_storage(&node).await?;
        let mounted = storage
            .mount_volume(node_proto::MountVolumeRequest {
                backend_handle: device,
                mount_point: req.mount_point,
                fs_type: req.fs_type.clone(),
                persistent: req.persistent,
            })
            .await
            .map_err(|e| Status::internal(format!("mounting volume: {e}")))?
            .into_inner();
        vol.mount_point = mounted.mount_point;
        if !req.fs_type.trim().is_empty() {
            vol.fs_type = req.fs_type.trim().to_ascii_lowercase();
        }
        vol.generation += 1;
        self.db
            .upsert_volume(&vol)
            .map_err(|e| Status::internal(e.to_string()))?;
        self.log_replication_event(
            &actor,
            Some("volume.update"),
            EVT_VOLUME_UPDATE,
            &format!("volume/{}", vol.name),
            self.host_volume_event(&vol),
        );
        Ok(Response::new(controller_proto::MountVolumeResponse {
            success: true,
            message: mounted.message,
            volume: Some(self.volume_info_from_row(&vol, None)),
        }))
    }

    async fn unmount_volume(
        &self,
        request: Request<controller_proto::UnmountVolumeRequest>,
    ) -> Result<Response<controller_proto::UnmountVolumeResponse>, Status> {
        self.require_operator(&request, OperatorRole::VmAdmin)?;
        let actor = Self::audit_actor(&request);
        let req = request.into_inner();
        let mut vol = self
            .resolve_volume_row(req.name.trim())?
            .ok_or_else(|| Status::not_found(format!("volume '{}' not found", req.name)))?;
        if !crate::host_volume::is_host(&vol.storage_class) {
            return Err(Status::failed_precondition(
                "unmount is supported for lvm and zfs volumes",
            ));
        }
        if vol.mount_point.is_empty() {
            return Ok(Response::new(controller_proto::UnmountVolumeResponse {
                success: true,
                message: format!("volume '{}' is not mounted", vol.name),
                volume: Some(self.volume_info_from_row(&vol, None)),
            }));
        }
        let node = self.host_volume_node(&vol).await?;
        let mut storage = self.connect_node_storage(&node).await?;
        let unmounted = storage
            .unmount_volume(node_proto::UnmountVolumeRequest {
                mount_point: vol.mount_point.clone(),
                persistent: req.persistent,
            })
            .await
            .map_err(|e| Status::internal(format!("unmounting volume: {e}")))?
            .into_inner();
        vol.mount_point.clear();
        vol.generation += 1;
        self.db
            .upsert_volume(&vol)
            .map_err(|e| Status::internal(e.to_string()))?;
        self.log_replication_event(
            &actor,
            Some("volume.update"),
            EVT_VOLUME_UPDATE,
            &format!("volume/{}", vol.name),
            self.host_volume_event(&vol),
        );
        Ok(Response::new(controller_proto::UnmountVolumeResponse {
            success: true,
            message: unmounted.message,
            volume: Some(self.volume_info_from_row(&vol, None)),
        }))
    }
    async fn register_node(
        &self,
        request: Request<controller_proto::RegisterNodeRequest>,
    ) -> Result<Response<controller_proto::RegisterNodeResponse>, Status> {
        auth::require_peer(&request, &[CN_NODE_PREFIX])?;
        let actor = Self::audit_actor(&request);
        let req = request.into_inner();
        let (cpu, mem) = req
            .capacity
            .map(|c| (c.cpu_cores, c.memory_bytes))
            .unwrap_or((0, 0));
        let storage_backend = normalize_storage_backend(req.storage_backend, false)?;

        let existing = self
            .db
            .get_node(&req.node_id)
            .map_err(|e| Status::internal(format!("checking node: {e}")))?;

        let approval_status = match &existing {
            Some(n) => n.approval_status.clone(),
            None => {
                if self.require_manual_approval {
                    "pending".to_string()
                } else {
                    "approved".to_string()
                }
            }
        };

        let dc_id = if req.dc_id.trim().is_empty() {
            existing
                .as_ref()
                .map(|n| n.dc_id.clone())
                .unwrap_or_default()
        } else {
            req.dc_id.clone()
        };

        // An agent restart re-registers; that must not undo a drain or cordon.
        let status = crate::db::node_status_after_register(
            existing.as_ref().map(|n| n.status.as_str()),
            &approval_status,
        );
        let node = NodeRow {
            id: req.node_id.clone(),
            hostname: req.hostname.clone(),
            address: req.address.clone(),
            cpu_cores: cpu,
            memory_bytes: mem,
            status: status.clone(),
            last_heartbeat: String::new(),
            gateway_interface: String::new(),
            cpu_used: 0,
            memory_used: 0,
            storage_backend,
            disable_vxlan: req.disable_vxlan,
            approval_status: approval_status.clone(),
            cert_expiry_days: req.cert_expiry_days,
            luks_method: req.luks_method.clone(),
            dc_id: dc_id.clone(),
        };

        self.db
            .upsert_node(&node)
            .map_err(|e| Status::internal(format!("storing node: {e}")))?;

        if !req.labels.is_empty() {
            self.db
                .upsert_node_labels(&req.node_id, &req.labels)
                .map_err(|e| Status::internal(format!("storing labels: {e}")))?;
        }

        self.log_replication_event_required(
            &actor,
            Some("RegisterNode"),
            EVT_NODE_REGISTER,
            &format!("node/{}", req.node_id),
            serde_json::json!({
                "nodeId": req.node_id,
                "hostname": req.hostname,
                "address": req.address,
                "cpuCores": req.capacity.as_ref().map(|c| c.cpu_cores).unwrap_or(0),
                "memoryBytes": req.capacity.as_ref().map(|c| c.memory_bytes).unwrap_or(0),
                "status": status,
                "gatewayInterface": "",
                "storageBackend": req.storage_backend,
                "disableVxlan": req.disable_vxlan,
                "certExpiryDays": req.cert_expiry_days,
                "approvalStatus": approval_status,
                "labels": req.labels,
                "luksMethod": req.luks_method,
                "dcId": dc_id,
            }),
        )?;

        if approval_status == "approved" {
            if let Err(e) = self.clients.connect(&req.address).await {
                warn!(address = %req.address, error = %e, "failed to connect to node");
            }
            info!(node_id = %req.node_id, address = %req.address, "registered node (approved)");
        } else {
            info!(node_id = %req.node_id, address = %req.address, approval_status = %approval_status, "node registered with pending approval");
        }

        let message = if approval_status == "approved" {
            "registered".to_string()
        } else {
            format!("registered (approval status: {approval_status})")
        };

        Ok(Response::new(controller_proto::RegisterNodeResponse {
            success: true,
            message,
            approval_status,
        }))
    }

    async fn heartbeat(
        &self,
        request: Request<controller_proto::HeartbeatRequest>,
    ) -> Result<Response<controller_proto::HeartbeatResponse>, Status> {
        auth::require_peer(&request, &[CN_NODE_PREFIX])?;
        let actor = Self::audit_actor(&request);
        let req = request.into_inner();
        let (cpu_used, mem_used) = req
            .usage
            .map(|u| (u.cpu_cores_used, u.memory_bytes_used))
            .unwrap_or((0, 0));

        let found = self
            .db
            .update_heartbeat(
                &req.node_id,
                cpu_used,
                mem_used,
                req.cert_expiry_days,
                &req.luks_method,
            )
            .map_err(|e| Status::internal(e.to_string()))?;

        if !found {
            return Err(Status::not_found(format!(
                "node {} not registered",
                req.node_id
            )));
        }
        let _ = self.db.mark_direct_seen(&req.node_id);
        let _ = self.db.clear_node_lost(&req.node_id);
        if req.host_health == "degraded" {
            let _ = self.db.set_node_host_health(&req.node_id, "degraded");
            if let Ok(Some(node)) = self.db.get_node(&req.node_id) {
                if crate::ha::proactive_should_cordon(&req.host_health, &node.status) {
                    let _ = self.db.update_node_status(&req.node_id, "cordoned");
                    let _ = self.db.insert_ha_event(
                        &req.node_id,
                        "",
                        "proactive",
                        &req.host_health_reason,
                    );
                }
            }
        } else if req.host_health == "unknown" {
            // Do not cordon. A peer probe on the HA pass decides whether the
            // OSD is actually down. One failed query never arrives here: the
            // agent reports it as ok until the failure repeats.
            let _ = self.db.set_node_host_health(&req.node_id, "unknown");
        } else if !req.host_health.is_empty() {
            let _ = self.db.set_node_host_health(&req.node_id, "ok");
        }
        for action in &req.ha_actions {
            let (phase, vm_id) = action
                .split_once(':')
                .unwrap_or(("action", action.as_str()));
            let _ = self.db.insert_ha_event(&req.node_id, vm_id, phase, action);
        }
        // Coming back is not enough. The blocklist stays until this host
        // reports that placement settled and every fenced guest is masked
        // or still assigned here.
        let assigned_names: Vec<String> = self
            .db
            .list_vms_for_node(&req.node_id)
            .unwrap_or_default()
            .into_iter()
            .map(|vm| vm.name)
            .collect();
        match self.db.get_node_fence_hold(&req.node_id) {
            Ok(hold) => {
                let mut actions = req.ha_actions.clone();
                if hold.pending {
                    match self
                        .db
                        .placement_release_reports(&req.node_id, &hold.lost_at)
                    {
                        Ok(prior) => {
                            actions = crate::ha::merge_placement_reports(&actions, &prior);
                        }
                        Err(e) => {
                            warn!(node_id = %req.node_id, error = %e, "could not read earlier placement reports");
                        }
                    }
                }
                match crate::ha::fence_release(hold.pending, &hold.vms, &assigned_names, &actions) {
                    crate::ha::FenceRelease::Keep => {}
                    crate::ha::FenceRelease::Hold { vms } => {
                        if vms != hold.vms {
                            if let Err(e) = self.db.set_node_fence_hold(&req.node_id, true, &vms) {
                                warn!(node_id = %req.node_id, error = %e, "could not update the fence hold");
                            }
                        }
                    }
                    crate::ha::FenceRelease::Unfence => {
                        // The blocklist stays until a Ceph member accepts the
                        // removal. Clearing the hold first dropped it on the
                        // floor when that call failed, and the next heartbeat
                        // had nothing left to retry.
                        let node_id = req.node_id.clone();
                        let this = self.clone();
                        tokio::spawn(async move {
                            if this.unfence_returned_node(&node_id).await {
                                if let Err(e) = this.db.set_node_fence_hold(&node_id, false, &[]) {
                                    warn!(node_id, error = %e, "could not clear the fence hold");
                                }
                            }
                        });
                    }
                }
            }
            Err(e) => {
                warn!(node_id = %req.node_id, error = %e, "could not read the fence hold; leaving the blocklist in place");
            }
        }

        let capabilities = req.update_capabilities.join(",");
        if let Err(e) = self.db.upsert_node_host_status(
            &req.node_id,
            &req.kcore_version,
            &req.booted_generation,
            &req.nixos_generation,
            &capabilities,
            req.schema_version,
            &req.nixpkgs_rev,
        ) {
            warn!(node_id = %req.node_id, error = %e, "heartbeat stored usage but not host generation");
        }

        if req.gpus_reported {
            let mut rows = Vec::new();
            for gpu in &req.gpus {
                let functions = match crate::pci::join_pci_devices(&gpu.pci_devices) {
                    Ok(joined) => joined,
                    Err(e) => {
                        warn!(node = %req.node_id, gpu = %gpu.name, error = %e, "ignoring GPU with a bad PCI address");
                        continue;
                    }
                };
                if gpu.name.trim().is_empty() {
                    continue;
                }
                let kind = gpu.kind.trim().to_ascii_lowercase();
                rows.push(crate::db::NodeGpuRow {
                    node_id: req.node_id.clone(),
                    name: gpu.name.trim().to_ascii_lowercase(),
                    family: gpu.family.trim().to_ascii_lowercase(),
                    model: gpu.model.clone(),
                    address: gpu.address.trim().to_ascii_lowercase(),
                    pci_devices: functions,
                    iommu_group: gpu.iommu_group,
                    assignable: gpu.assignable,
                    blocked_reason: gpu.blocked_reason.clone(),
                    kind: if kind.is_empty() { "gpu".into() } else { kind },
                    role: gpu.role.trim().to_ascii_lowercase(),
                    class_code: gpu.class_code.trim().to_ascii_lowercase(),
                    characteristics: gpu.characteristics.clone(),
                    driver: gpu.driver.trim().to_string(),
                    parent: gpu.parent.trim().to_ascii_lowercase(),
                    profile: gpu.profile.trim().to_string(),
                    profiles: gpu
                        .profiles
                        .iter()
                        .map(|profile| profile.trim())
                        .filter(|profile| !profile.is_empty())
                        .map(|profile| profile.to_string())
                        .collect::<Vec<_>>()
                        .join(","),
                });
            }
            self.db
                .replace_node_gpus(&req.node_id, &rows)
                .map_err(|e| Status::internal(format!("storing GPU inventory: {e}")))?;
        }

        if let Ok(Some(node)) = self.db.get_node(&req.node_id) {
            if node.status == "ready" && self.db.node_needs_config_push(&node.id).unwrap_or(false) {
                match self.push_config_to_node(&node).await {
                    Ok(()) => {
                        if let Err(e) = self.db.clear_node_config_push(&node.id) {
                            warn!(node_id = %node.id, error = %e, "config refreshed but the pending flag remains");
                        }
                    }
                    Err(e) => {
                        warn!(
                            node_id = %node.id,
                            error = %e,
                            "node is ready again but the config push that drops failed-over VMs failed"
                        );
                    }
                }
            }
            let labels = self.db.get_node_labels(&req.node_id).unwrap_or_default();
            self.log_replication_event_required(
                &actor,
                None,
                EVT_NODE_HEARTBEAT,
                &format!("node/{}", req.node_id),
                serde_json::json!({
                    "nodeId": node.id,
                    "hostname": node.hostname,
                    "address": node.address,
                    "cpuCores": node.cpu_cores,
                    "memoryBytes": node.memory_bytes,
                    "status": node.status,
                    "gatewayInterface": node.gateway_interface,
                    "storageBackend": match node.storage_backend.as_str() {
                        "lvm" => controller_proto::StorageBackendType::Lvm as i32,
                        "zfs" => controller_proto::StorageBackendType::Zfs as i32,
                        _ => controller_proto::StorageBackendType::Filesystem as i32,
                    },
                    "disableVxlan": node.disable_vxlan,
                    "certExpiryDays": req.cert_expiry_days,
                    "approvalStatus": node.approval_status,
                    "labels": labels,
                    "luksMethod": req.luks_method,
                    "cpuUsed": cpu_used,
                    "memoryUsed": mem_used,
                    "lastHeartbeat": node.last_heartbeat,
                }),
            )?;
        }

        let assigned = self.db.list_vms_for_node(&req.node_id).unwrap_or_default();
        let policies = self.db.list_vm_ha().unwrap_or_default();
        let vm_monitors = assigned
            .iter()
            .filter_map(|vm| {
                let row = policies.iter().find(|row| row.vm_id == vm.id)?;
                if row.vm_monitoring != "restart" {
                    return None;
                }
                Some(controller_proto::VmMonitor {
                    vm_name: vm.name.clone(),
                    restart_after_secs: i32::try_from(row.vm_monitoring_secs).unwrap_or(0),
                })
            })
            .collect();
        // Names still on the hold, after this heartbeat's release decision.
        // The agent masks them even when its placement file no longer lists
        // the guest. An empty list is a host with no fence.
        let fence_vm_ids = match self.db.get_node_fence_hold(&req.node_id) {
            Ok(hold) if hold.pending => hold.vms,
            _ => Vec::new(),
        };
        Ok(Response::new(controller_proto::HeartbeatResponse {
            success: true,
            assigned_vm_ids: assigned.into_iter().map(|vm| vm.name).collect(),
            assignment_confirmed: true,
            vm_monitors,
            fence_vm_ids,
        }))
    }

    async fn sync_vm_state(
        &self,
        request: Request<controller_proto::SyncVmStateRequest>,
    ) -> Result<Response<controller_proto::SyncVmStateResponse>, Status> {
        auth::require_peer(&request, &[CN_NODE_PREFIX])?;
        let req = request.into_inner();
        info!(
            node_id = %req.node_id,
            vm_count = req.vms.len(),
            "syncing VM state from node"
        );

        for vm in &req.vms {
            let state_str = match controller_proto::VmState::try_from(vm.state) {
                Ok(controller_proto::VmState::Running) => "running",
                Ok(controller_proto::VmState::Stopped) => "stopped",
                Ok(controller_proto::VmState::Paused) => "paused",
                Ok(controller_proto::VmState::Error) => "error",
                _ => "unknown",
            };
            if vm.guest_heartbeat_unix > 0 {
                let _ = self.db.set_guest_heartbeat_for_name(
                    &req.node_id,
                    &vm.name,
                    vm.guest_heartbeat_unix,
                );
            }
            match self
                .db
                .apply_vm_runtime_state(&req.node_id, &vm.name, state_str)
            {
                Ok(crate::db::VmRuntimeApply::Changed(change)) => {
                    self.replicate_vm_observation(&change.vm_id, &change.current);
                    self.webhooks.emit(
                        crate::webhooks::EVENT_VM_STATE_CHANGED,
                        format!("vm/{}", change.name),
                        serde_json::json!({
                            "vmId": change.vm_id,
                            "name": change.name,
                            "nodeId": change.node_id,
                            "previousState": change.previous,
                            "state": change.current,
                        }),
                    );
                }
                Ok(crate::db::VmRuntimeApply::Unchanged) => {}
                Ok(crate::db::VmRuntimeApply::Missing) => {
                    warn!(
                        node_id = %req.node_id,
                        vm_name = %vm.name,
                        "node reported VM not tracked by controller (orphan)"
                    );
                }
                Err(e) => {
                    error!(
                        node_id = %req.node_id,
                        vm_name = %vm.name,
                        error = %e,
                        "failed to update VM runtime state"
                    );
                }
            }
        }

        Ok(Response::new(controller_proto::SyncVmStateResponse {
            success: true,
        }))
    }

    async fn sync_workload_state(
        &self,
        request: Request<controller_proto::SyncWorkloadStateRequest>,
    ) -> Result<Response<controller_proto::SyncWorkloadStateResponse>, Status> {
        auth::require_peer(&request, &[CN_NODE_PREFIX])?;
        let req = request.into_inner();
        for workload in &req.workloads {
            let state = match controller_proto::WorkloadKind::try_from(workload.kind)
                .unwrap_or(controller_proto::WorkloadKind::Unspecified)
            {
                controller_proto::WorkloadKind::Vm => {
                    match controller_proto::VmState::try_from(workload.vm_state)
                        .unwrap_or(controller_proto::VmState::Unknown)
                    {
                        controller_proto::VmState::Running => "running",
                        controller_proto::VmState::Stopped => "stopped",
                        controller_proto::VmState::Paused => "paused",
                        controller_proto::VmState::Error => "error",
                        controller_proto::VmState::Unknown => "unknown",
                    }
                }
                controller_proto::WorkloadKind::Container => {
                    match controller_proto::ContainerState::try_from(workload.container_state)
                        .unwrap_or(controller_proto::ContainerState::Unknown)
                    {
                        controller_proto::ContainerState::Created => "created",
                        controller_proto::ContainerState::Running => "running",
                        controller_proto::ContainerState::Stopped => "stopped",
                        controller_proto::ContainerState::Error => "error",
                        controller_proto::ContainerState::Unknown => "unknown",
                    }
                }
                controller_proto::WorkloadKind::Unspecified => "unknown",
            };
            let _ = self.db.update_workload_runtime_state(&workload.id, state);
        }
        Ok(Response::new(controller_proto::SyncWorkloadStateResponse {
            success: true,
        }))
    }

    async fn create_vm(
        &self,
        request: Request<controller_proto::CreateVmRequest>,
    ) -> Result<Response<controller_proto::CreateVmResponse>, Status> {
        self.require_operator(&request, OperatorRole::BackupOperator)?;
        let actor = Self::audit_actor(&request);
        let mut req = request.into_inner();
        let spec = req
            .spec
            .take()
            .ok_or_else(|| Status::invalid_argument("spec is required"))?;
        let mut pci_devices =
            crate::pci::join_pci_devices(&spec.pci_devices).map_err(Status::invalid_argument)?;
        let wants_gpu = spec.gpus.iter().any(|gpu| !gpu.trim().is_empty());
        let wants_vgpu = spec.vgpus.iter().any(|gpu| !gpu.trim().is_empty());
        let modes =
            usize::from(wants_gpu) + usize::from(wants_vgpu) + usize::from(!pci_devices.is_empty());
        if modes > 1 {
            return Err(Status::invalid_argument(
                "pass a whole device (--gpu), a vGPU slice (--vgpu), or a raw PCI address, not more than one",
            ));
        }
        let required_labels = req
            .node_labels
            .iter()
            .map(|label| crate::scheduler::normalize_label(label).map_err(Status::invalid_argument))
            .collect::<Result<Vec<_>, _>>()?;
        let anti_affinity = crate::scheduler::normalize_anti_affinity(&req.anti_affinity)
            .map_err(Status::invalid_argument)?;

        // Upsert: if a VM with the requested name already exists, diff the
        // incoming spec against the stored row and apply any mutable changes
        // (rejecting immutable diffs). This makes CreateVm idempotent so
        // `kctl apply -f` is a no-op when nothing changed.
        let name_key = spec.name.trim().to_string();
        if !name_key.is_empty() {
            if let Some(existing) = self
                .db
                .list_vms()
                .map_err(|e| Status::internal(format!("listing vms for upsert: {e}")))?
                .into_iter()
                .find(|v| v.name == name_key)
            {
                return self.upsert_existing_vm(&actor, req, spec, existing).await;
            }
        }

        let requested_storage_backend = normalize_storage_backend(req.storage_backend, true)?;
        let requested_storage_size_bytes = validate_storage_size_bytes(req.storage_size_bytes)?;
        if (!pci_devices.is_empty() || wants_gpu || wants_vgpu) && req.target_node.trim().is_empty()
        {
            return Err(Status::invalid_argument(
                "a PCI assignment requires target_node; the controller does not place passthrough devices automatically",
            ));
        }

        let target_node_requested = !req.target_node.is_empty();
        let mut node = if target_node_requested {
            let node = self
                .db
                .get_node_by_address(&req.target_node)
                .map_err(|e| Status::internal(e.to_string()))?
                .or_else(|| self.db.get_node(&req.target_node).ok().flatten())
                .ok_or_else(|| Status::not_found(format!("node {} not found", req.target_node)))?;
            self.ensure_explicit_placement(&node, &required_labels, &anti_affinity)?;
            node
        } else {
            let nodes = self
                .db
                .list_nodes()
                .map_err(|e| Status::internal(e.to_string()))?;
            let compatible_nodes: Vec<NodeRow> = nodes
                .into_iter()
                .filter(|n| self.node_supports_backend(n, &requested_storage_backend))
                .collect();
            let target_dc = req.target_dc.trim();
            self.select_scheduled_node(
                &compatible_nodes,
                spec.cpu,
                spec.memory_bytes,
                target_dc,
                &required_labels,
                &anti_affinity,
            )?
            .ok_or_else(|| {
                if !anti_affinity.is_empty() {
                    Status::unavailable(format!(
                        "no ready node can place another VM in anti-affinity group '{anti_affinity}'"
                    ))
                } else if !required_labels.is_empty() {
                    Status::unavailable(format!(
                        "no ready node with labels [{}] has sufficient capacity",
                        required_labels.join(", ")
                    ))
                } else if target_dc.is_empty() {
                    Status::unavailable(
                        "no ready node with sufficient capacity matching requested storage backend",
                    )
                } else {
                    Status::unavailable(format!(
                        "no ready node in DC '{}' with sufficient capacity matching requested storage backend",
                        target_dc,
                    ))
                }
            })?
        };
        if target_node_requested {
            let preflight_error = if !self.node_supports_backend(&node, &requested_storage_backend)
            {
                Some(Status::failed_precondition(format!(
                    "VM storage backend '{}' does not match node '{}' backend '{}'",
                    requested_storage_backend, node.id, node.storage_backend
                )))
            } else {
                self.preflight_vm_create_on_node(&node, &spec, &requested_storage_backend)
                    .err()
            };
            if let Some(err) = preflight_error {
                if !pci_devices.is_empty() || wants_gpu || wants_vgpu {
                    return Err(err);
                }
                if let Some(fallback) = self
                    .select_scheduled_node(
                        &self.alternative_vm_create_nodes(
                            &node.id,
                            &requested_storage_backend,
                            spec.cpu,
                            spec.memory_bytes,
                        ),
                        spec.cpu,
                        spec.memory_bytes,
                        "",
                        &required_labels,
                        &anti_affinity,
                    )?
                    .clone()
                {
                    warn!(
                        vm_name = %spec.name,
                        requested_node = %node.id,
                        fallback_node = %fallback.id,
                        reason = %err.message(),
                        "target node failed preflight; auto-falling back to alternative node"
                    );
                    node = fallback;
                } else {
                    return Err(err);
                }
            }
        } else if !self.node_supports_backend(&node, &requested_storage_backend) {
            return Err(Status::failed_precondition(format!(
                "VM storage backend '{}' does not match node '{}' backend '{}'",
                requested_storage_backend, node.id, node.storage_backend
            )));
        }
        self.preflight_vm_create_on_node(&node, &spec, &requested_storage_backend)?;
        let ha_policy = crate::ha::VmPolicy {
            restart_priority: req.restart_priority.clone(),
            vm_monitoring: req.vm_monitoring.clone(),
            vm_monitoring_secs: i64::from(req.vm_monitoring_secs),
            replica_node: req.replica_node.clone(),
            replica_interval_secs: i64::from(req.replica_interval_secs),
        }
        .normalized()
        .map_err(Status::invalid_argument)?;
        if !ha_policy.replica_node.is_empty() && requested_storage_backend != "zfs" {
            return Err(Status::invalid_argument(
                "a replica node is only supported for zfs VMs",
            ));
        }
        if !ha_policy.replica_node.is_empty() && ha_policy.replica_node == node.id {
            return Err(Status::invalid_argument(
                "replica node must be a different host",
            ));
        }
        let create_report = self.admission_report(
            None,
            Some((
                node.id.as_str(),
                spec.cpu,
                spec.memory_bytes,
                requested_storage_backend.as_str(),
                ha_policy.replica_node.as_str(),
            )),
        )?;
        self.enforce_admission(
            &create_report,
            req.ignore_admission_control,
            &actor,
            &format!("vm/{}", spec.name),
        )?;

        let mut assigned_gpus = String::new();
        if wants_gpu {
            let (names, functions) = self.assign_gpus_on_node(&node.id, &spec.gpus, None)?;
            assigned_gpus = names;
            pci_devices = functions;
        } else if wants_vgpu {
            let (names, functions) = self.assign_vgpus_on_node(&node.id, &spec.vgpus, None)?;
            assigned_gpus = names;
            pci_devices = functions;
        }

        let vm_id = if spec.id.is_empty() {
            let mut selected: Option<String> = None;
            for _ in 0..8 {
                let candidate = format!("vm-{}", short_vm_id_seed());
                let exists = self
                    .db
                    .get_vm(&candidate)
                    .map_err(|e| Status::internal(format!("checking vm id: {e}")))?
                    .is_some();
                if !exists {
                    selected = Some(candidate);
                    break;
                }
            }
            selected.ok_or_else(|| Status::internal("failed to allocate unique vm id"))?
        } else {
            if self
                .db
                .get_vm(&spec.id)
                .map_err(|e| Status::internal(format!("checking vm id: {e}")))?
                .is_some()
            {
                return Err(Status::already_exists(format!(
                    "vm {} already exists",
                    spec.id
                )));
            }
            spec.id.clone()
        };

        let vm_name = if spec.name.is_empty() {
            vm_id.clone()
        } else {
            spec.name.clone()
        };

        if self
            .db
            .find_node_for_vm(&vm_name)
            .map_err(|e| Status::internal(format!("checking vm name: {e}")))?
            .is_some()
        {
            return Err(Status::already_exists(format!(
                "vm name {vm_name} already exists"
            )));
        }

        let image_url_input = req.image_url.trim();
        let image_path_input = req.image_path.trim();
        let boot_volume = match vault_boot_volume_name(&spec) {
            Some(name) => Some(
                self.resolve_volume_row(&name)?
                    .ok_or_else(|| Status::not_found(format!("boot volume '{name}' not found")))?,
            ),
            None => None,
        };
        if boot_volume.is_none() && image_url_input.is_empty() && image_path_input.is_empty() {
            return Err(Status::invalid_argument(
                "either image_url or image_path is required",
            ));
        }
        if boot_volume.is_none() && !image_url_input.is_empty() && !image_path_input.is_empty() {
            return Err(Status::invalid_argument(
                "image_url and image_path are mutually exclusive",
            ));
        }

        let (image_url, image_sha256, image_path, image_format) = if let Some(ref vol) = boot_volume
        {
            if vol.storage_class != "ceph" || vol.pool.is_empty() || vol.image.is_empty() {
                return Err(Status::failed_precondition(
                    "boot test volume must be a Ceph volume with a pool and image",
                ));
            }
            (
                String::new(),
                String::new(),
                VAULT_BOOT_IMAGE.to_string(),
                "raw".to_string(),
            )
        } else if !image_url_input.is_empty() {
            let image_url = validate_image_url(image_url_input)?;
            let image_sha256 = validate_image_sha256(&req.image_sha256)?;
            let image_path = derive_local_image_path(&image_url, &image_sha256);
            let image_format = derive_image_format(&image_url);
            (image_url, image_sha256, image_path, image_format)
        } else {
            let image_path = validate_image_path(image_path_input)?;
            let image_format = if req.image_format.trim().is_empty() {
                derive_image_format_from_path(&image_path)
            } else {
                normalize_image_format(&req.image_format)?
            };
            (String::new(), String::new(), image_path, image_format)
        };
        let existing_on_node = self
            .db
            .list_vms_for_node(&node.id)
            .map_err(|e| Status::internal(format!("listing vms for image collision check: {e}")))?;
        if let Some((addr, owner)) = crate::pci::first_overlap(
            &pci_devices,
            existing_on_node
                .iter()
                .map(|existing| (existing.name.as_str(), existing.pci_devices.as_str())),
        ) {
            return Err(Status::already_exists(format!(
                "PCI device {addr} is already assigned to VM '{owner}' on node '{}'",
                node.id
            )));
        }
        if let Some(conflict) = existing_on_node.into_iter().find(|existing| {
            existing.image_path == image_path
                && image_used_in_place(&existing.storage_backend)
                && image_used_in_place(&requested_storage_backend)
        }) {
            return Err(Status::failed_precondition(format!(
                "image path '{}' is already used by VM '{}' on node '{}'; duplicate writable disk usage is not supported",
                image_path, conflict.name, node.id
            )));
        }
        let declared = crate::vm_nics::normalize_declared_nics(&spec.nics)?;
        for nic in &declared {
            if nic.network != "default"
                && self
                    .db
                    .get_network_for_node(&node.id, &nic.network)
                    .map_err(|e| Status::internal(format!("checking network: {e}")))?
                    .is_none()
            {
                return Err(Status::failed_precondition(format!(
                    "network '{}' is not configured on node '{}'",
                    nic.network, node.id
                )));
            }
        }
        let vm_network = declared[0].network.clone();
        let vm_ip = self.allocate_nic_address(&node.id, &vm_network, &vm_id)?;

        // Honor declarative desired_state when supplied; default to running.
        let desired_auto_start =
            match controller_proto::VmDesiredState::try_from(spec.desired_state)
                .unwrap_or(controller_proto::VmDesiredState::Unspecified)
            {
                controller_proto::VmDesiredState::Stopped => false,
                controller_proto::VmDesiredState::Running
                | controller_proto::VmDesiredState::Unspecified => true,
            };

        let vm = VmRow {
            id: vm_id.clone(),
            name: vm_name,
            cpu: spec.cpu,
            memory_bytes: spec.memory_bytes,
            image_path,
            image_url,
            image_sha256,
            image_format,
            image_size: 8192,
            network: vm_network,
            auto_start: desired_auto_start,
            node_id: node.id.clone(),
            created_at: String::new(),
            runtime_state: "unknown".to_string(),
            cloud_init_user_data: req.cloud_init_user_data,
            storage_backend: requested_storage_backend,
            storage_size_bytes: requested_storage_size_bytes,
            vm_ip,
            pci_devices,
            labels: serde_json::to_string(&spec.labels).unwrap_or_else(|_| "{}".into()),
        };

        if vm.storage_backend == "ceph" {
            if !self.is_healthy_ceph_member(&node.id)? {
                return Err(Status::failed_precondition(
                    "storage_backend ceph requires a healthy CephCluster that includes the target node",
                ));
            }
            // A boot test already wrote the guest into this RBD. Creating
            // another image would boot an empty disk.
            if boot_volume.is_none() {
                if self.clients.get_storage(&node.address).is_none() {
                    self.clients.connect(&node.address).await.map_err(|e| {
                        Status::unavailable(format!("connecting to Ceph node: {e}"))
                    })?;
                }
                let mut storage = self
                    .clients
                    .get_storage(&node.address)
                    .ok_or_else(|| Status::unavailable("Ceph storage client unavailable"))?;
                storage
                    .create_volume(node_proto::CreateVolumeRequest {
                        volume_id: format!("kcore-{}", vm.id),
                        storage_class: "ceph".into(),
                        size_bytes: vm.storage_size_bytes,
                        parameters: HashMap::new(),
                        lv_name: String::new(),
                    })
                    .await
                    .map_err(|e| Status::internal(format!("creating RBD volume: {e}")))?;
            }
        }

        if let Err(e) = self.db.insert_vm(&vm) {
            if vm.storage_backend == "ceph" {
                self.rollback_created_vm(&node, &vm).await;
            }
            return Err(Status::internal(format!("storing vm: {e}")));
        }
        let ha_row = crate::db::VmHaRow {
            vm_id: vm.id.clone(),
            restart_priority: ha_policy.restart_priority,
            vm_monitoring: ha_policy.vm_monitoring,
            vm_monitoring_secs: ha_policy.vm_monitoring_secs,
            replica_node: ha_policy.replica_node,
            replica_interval_secs: ha_policy.replica_interval_secs,
            last_replica_at: 0,
            last_replica_snapshot: String::new(),
            last_guest_heartbeat: 0,
        };
        let _ = self.db.upsert_vm_ha(&ha_row);
        if !assigned_gpus.is_empty() {
            if let Err(e) = self.db.set_vm_gpu(&vm.id, &assigned_gpus) {
                self.rollback_created_vm(&node, &vm).await;
                return Err(Status::internal(format!("storing GPU assignment: {e}")));
            }
        }
        if let Err(e) = self.assign_ipv6_if_configured(&node.id, &vm.network, &vm_id, 0) {
            self.rollback_created_vm(&node, &vm).await;
            return Err(e);
        }
        for (index, nic) in declared.iter().skip(1).enumerate() {
            let ip = match self.allocate_nic_address(&node.id, &nic.network, &vm_id) {
                Ok(ip) => ip,
                Err(e) => {
                    self.rollback_created_vm(&node, &vm).await;
                    return Err(e);
                }
            };
            let position = (index + 1) as i32;
            if let Err(e) = self.db.insert_vm_nic(&VmNicRow {
                vm_id: vm_id.clone(),
                position,
                network: nic.network.clone(),
                mac_address: nic.mac_address.clone(),
                model: nic.model.clone(),
                ip_address: ip,
            }) {
                self.rollback_created_vm(&node, &vm).await;
                return Err(Status::internal(format!("storing extra NIC: {e}")));
            }
            if let Err(e) = self.assign_ipv6_if_configured(&node.id, &nic.network, &vm_id, position)
            {
                self.rollback_created_vm(&node, &vm).await;
                return Err(e);
            }
        }
        if !anti_affinity.is_empty() {
            if let Err(e) = self.db.set_vm_anti_affinity(&vm_id, &anti_affinity) {
                self.rollback_created_vm(&node, &vm).await;
                return Err(Status::internal(format!("storing anti-affinity: {e}")));
            }
        }
        if vm.storage_backend == "ceph" {
            let mut root = VolumeRow::new_root(&vm.id, &vm.name, vm.storage_size_bytes);
            if let Some(ref vol) = boot_volume {
                root.pool.clone_from(&vol.pool);
                root.image.clone_from(&vol.image);
                root.size_bytes = vol.size_bytes;
            }
            if let Err(e) = self.db.upsert_volume(&root) {
                self.rollback_created_vm(&node, &vm).await;
                return Err(Status::internal(format!("storing Ceph volume: {e}")));
            }
        }

        if !req.ssh_key_names.is_empty() {
            for key_name in &req.ssh_key_names {
                if self
                    .db
                    .get_ssh_key(key_name)
                    .map_err(|e| Status::internal(format!("checking ssh key: {e}")))?
                    .is_none()
                {
                    self.rollback_created_vm(&node, &vm).await;
                    return Err(Status::not_found(format!(
                        "SSH key '{}' not found",
                        key_name
                    )));
                }
            }
            if let Err(e) = self.db.associate_vm_ssh_keys(&vm_id, &req.ssh_key_names) {
                self.rollback_created_vm(&node, &vm).await;
                return Err(Status::internal(format!("associating ssh keys: {e}")));
            }
        }

        info!(vm_id = %vm_id, node_id = %node.id, "created VM, pushing config");

        // Wait for the rebuild: without the barrier a failing `nixos-rebuild`
        // left the DB claiming a VM the node never built, and the rollback
        // below could never fire.
        if let Err(push_err) = self.push_config_and_await_apply(&node).await {
            warn!(
                vm_id = %vm_id,
                node_id = %node.id,
                error = %push_err,
                "failed to apply config after VM insert; rolling back VM row"
            );
            self.rollback_created_vm(&node, &vm).await;
            return Err(Status::aborted(format!(
                "failed to apply VM {} on node {}: {}",
                vm_id,
                node.id,
                push_err.message()
            )));
        }

        let ssh_key_names: Vec<String> = self
            .db
            .get_vm_ssh_key_names(&vm_id)
            .map_err(|e| Status::internal(format!("listing VM SSH keys for replication: {e}")))?;
        let extra_nics = self
            .db
            .list_vm_nics(&vm_id)
            .map_err(|e| Status::internal(format!("listing extra NICs for replication: {e}")))?;
        let extra_nic_body: Vec<serde_json::Value> = extra_nics
            .iter()
            .map(|nic| {
                serde_json::json!({
                    "position": nic.position,
                    "network": nic.network,
                    "macAddress": nic.mac_address,
                    "model": nic.model,
                    "ipAddress": nic.ip_address,
                })
            })
            .collect();
        let ipv6_body: Vec<serde_json::Value> = self
            .db
            .list_vm_ipv6(&vm_id)
            .map_err(|e| Status::internal(format!("listing IPv6 addresses for replication: {e}")))?
            .iter()
            .map(|row| {
                serde_json::json!({
                    "position": row.position,
                    "address": row.address,
                })
            })
            .collect();

        self.webhooks.emit(
            crate::webhooks::EVENT_VM_CREATED,
            format!("vm/{vm_id}"),
            serde_json::json!({ "vmId": vm_id, "name": vm.name, "labels": vm.labels }),
        );
        self.log_replication_event(
            &actor,
            Some("CreateVm"),
            EVT_VM_CREATE,
            &format!("vm/{vm_id}"),
            serde_json::json!({
                "vmId": vm_id,
                "nodeId": node.id,
                "name": vm.name,
                "cpu": vm.cpu,
                "memoryBytes": vm.memory_bytes,
                "imagePath": vm.image_path,
                "imageUrl": vm.image_url,
                "imageSha256": vm.image_sha256,
                "imageFormat": vm.image_format,
                "imageSize": vm.image_size,
                "network": vm.network,
                "autoStart": vm.auto_start,
                "runtimeState": vm.runtime_state,
                "cloudInitUserData": vm.cloud_init_user_data,
                "storageBackend": vm.storage_backend,
                "storageSizeBytes": vm.storage_size_bytes,
                "vmIp": vm.vm_ip,
                "pciDevices": crate::pci::split_pci_devices(&vm.pci_devices),
                "labels": vm.labels,
                "gpuName": assigned_gpus,
                "sshKeyNames": ssh_key_names,
                "extraNics": extra_nic_body,
                "ipv6Addresses": ipv6_body,
                "antiAffinity": anti_affinity,
                "restartPriority": ha_row.restart_priority,
                "vmMonitoring": ha_row.vm_monitoring,
                "vmMonitoringSecs": ha_row.vm_monitoring_secs,
                "replicaNode": ha_row.replica_node,
                "replicaIntervalSecs": ha_row.replica_interval_secs,
                "lastReplicaAt": ha_row.last_replica_at,
                "lastReplicaSnapshot": ha_row.last_replica_snapshot,
            }),
        );

        Ok(Response::new(controller_proto::CreateVmResponse {
            vm_id,
            node_id: node.id,
            state: controller_proto::VmState::Stopped as i32,
            action: controller_proto::ApplyAction::Created as i32,
            changed_fields: Vec::new(),
        }))
    }

    async fn update_vm(
        &self,
        request: Request<controller_proto::UpdateVmRequest>,
    ) -> Result<Response<controller_proto::UpdateVmResponse>, Status> {
        self.require_operator(&request, OperatorRole::VmAdmin)?;
        let actor = Self::audit_actor(&request);
        let req = request.into_inner();

        if req.vm_id.is_empty() {
            return Err(Status::invalid_argument("vm_id is required"));
        }

        let node = self.resolve_node_for_vm(&req.vm_id, &req.target_node)?;

        let cpu = if req.cpu > 0 { Some(req.cpu) } else { None };
        let mem = if req.memory_bytes > 0 {
            Some(req.memory_bytes)
        } else {
            None
        };

        if cpu.is_none() && mem.is_none() && req.labels.is_empty() {
            return Err(Status::invalid_argument(
                "at least one of cpu, memory_bytes, or labels must be set",
            ));
        }

        if cpu.is_some() || mem.is_some() {
            let updated = self
                .db
                .update_vm_spec(&req.vm_id, cpu, mem)
                .map_err(|e| Status::internal(format!("updating vm: {e}")))?;
            if !updated {
                return Err(Status::not_found(format!("VM '{}' not found", req.vm_id)));
            }
        }

        let mut labels_body = serde_json::Value::Null;
        if !req.labels.is_empty() {
            let existing = self
                .db
                .get_vm(&req.vm_id)
                .map_err(|e| Status::internal(e.to_string()))?
                .or_else(|| {
                    self.db
                        .list_vms()
                        .ok()
                        .and_then(|rows| rows.into_iter().find(|v| v.name == req.vm_id))
                })
                .ok_or_else(|| Status::not_found(format!("VM '{}' not found", req.vm_id)))?;
            let mut labels = vm_labels(&existing.labels);
            for (key, value) in &req.labels {
                labels.insert(key.clone(), value.clone());
            }
            let encoded = serde_json::to_string(&labels).unwrap_or_else(|_| "{}".into());
            let updated = self
                .db
                .set_vm_labels(&existing.id, &encoded)
                .map_err(|e| Status::internal(format!("storing labels: {e}")))?;
            if !updated {
                return Err(Status::not_found(format!("VM '{}' not found", req.vm_id)));
            }
            labels_body = serde_json::json!(labels);
        }

        info!(vm_id = %req.vm_id, cpu = ?cpu, memory_bytes = ?mem, "updated VM spec, pushing config");
        self.push_config_to_node(&node).await?;
        self.log_replication_event(
            &actor,
            Some("UpdateVm"),
            EVT_VM_UPDATE,
            &format!("vm/{}", req.vm_id),
            serde_json::json!({
                "vmId": req.vm_id,
                "nodeId": node.id,
                "cpu": cpu,
                "memoryBytes": mem,
                "labels": labels_body,
            }),
        );

        Ok(Response::new(controller_proto::UpdateVmResponse {
            success: true,
            message: format!("VM '{}' updated", req.vm_id),
        }))
    }

    async fn delete_vm(
        &self,
        request: Request<controller_proto::DeleteVmRequest>,
    ) -> Result<Response<controller_proto::DeleteVmResponse>, Status> {
        self.require_operator(&request, OperatorRole::BackupOperator)?;
        let role = crate::auth::controller_operator_role(
            &request,
            &self.db,
            self.tls_paths.is_some(),
            self.bootstrap_kctl,
        )?;
        let actor = Self::audit_actor(&request);
        let req = request.into_inner();
        let node = self.resolve_node_for_vm(&req.vm_id, &req.target_node)?;
        let db_vm = self
            .db
            .get_vm(&req.vm_id)
            .map_err(|e| Status::internal(e.to_string()))?
            .or_else(|| {
                self.db
                    .list_vms()
                    .ok()
                    .and_then(|rows| rows.into_iter().find(|v| v.name == req.vm_id))
            })
            .ok_or_else(|| Status::not_found(format!("VM '{}' not found", req.vm_id)))?;
        if !role.satisfies(OperatorRole::VmAdmin) && !restore_labelled(&db_vm.labels) {
            return Err(Status::permission_denied(
                "backup-operator can delete only VMs labelled kcore-vault/restore",
            ));
        }
        let volumes = self
            .db
            .list_volumes_for_vm(&db_vm.id)
            .map_err(|e| Status::internal(e.to_string()))?;

        // `rbd rm` cannot remove an image the owning node still has mapped, and
        // the config push that stops the guest only happens at the end of this
        // RPC. Deleting the volume first therefore left the RBD image behind in
        // the pool with its bookkeeping row already gone. Stop the guest and
        // unmap first; an unreachable node is tolerated so a dead host can
        // still be cleaned up.
        self.cold_release_ceph_vm(&db_vm, &node).await?;
        self.lock_encrypted_volumes_on_node(&node, &volumes).await;

        let deleted = self
            .db
            .delete_vm_by_id_or_name(&db_vm.id)
            .map_err(|e| Status::internal(format!("deleting vm: {e}")))?;
        if !deleted {
            return Err(Status::not_found(format!("VM '{}' not found", req.vm_id)));
        }

        let delete_data = req.delete_data_volumes;
        if !volumes.is_empty() {
            if self.clients.get_storage(&node.address).is_none() {
                let _ = self.clients.connect(&node.address).await;
            }
            if let Some(mut storage) = self.clients.get_storage(&node.address) {
                for vol in &volumes {
                    if vol.role == VolumeRow::ROLE_DATA && !delete_data {
                        continue;
                    }
                    let handle = format!("{}/{}", vol.pool, vol.image);
                    if let Err(e) = storage
                        .delete_volume(node_proto::DeleteVolumeRequest {
                            backend_handle: handle,
                        })
                        .await
                    {
                        warn!(vm_id = %db_vm.id, volume = %vol.name, error = %e, "failed to delete RBD volume");
                    }
                }
            }
            if delete_data {
                self.db
                    .delete_volume_by_vm(&db_vm.id)
                    .map_err(|e| Status::internal(format!("deleting volume rows: {e}")))?;
            } else {
                self.db
                    .detach_data_volumes_for_vm(&db_vm.id)
                    .map_err(|e| Status::internal(format!("detaching data volumes: {e}")))?;
                self.db
                    .delete_root_volume_by_vm(&db_vm.id)
                    .map_err(|e| Status::internal(format!("deleting root volume row: {e}")))?;
            }
        }

        info!(vm_id = %db_vm.id, node_id = %node.id, "deleted VM, pushing config");

        self.push_config_to_node(&node).await?;
        self.webhooks.emit(
            crate::webhooks::EVENT_VM_DELETED,
            format!("vm/{}", db_vm.id),
            serde_json::json!({ "vmId": db_vm.id, "nodeId": node.id }),
        );
        self.log_replication_event(
            &actor,
            Some("DeleteVm"),
            EVT_VM_DELETE,
            &format!("vm/{}", db_vm.id),
            serde_json::json!({
                "vmId": db_vm.id,
                "nodeId": node.id,
            }),
        );

        Ok(Response::new(controller_proto::DeleteVmResponse {
            success: true,
        }))
    }

    async fn set_vm_desired_state(
        &self,
        request: Request<controller_proto::SetVmDesiredStateRequest>,
    ) -> Result<Response<controller_proto::SetVmDesiredStateResponse>, Status> {
        self.require_operator(&request, OperatorRole::BackupOperator)?;
        let actor = Self::audit_actor(&request);
        let req = request.into_inner();
        let auto_start = match controller_proto::VmDesiredState::try_from(req.desired_state)
            .unwrap_or(controller_proto::VmDesiredState::Unspecified)
        {
            controller_proto::VmDesiredState::Running => true,
            controller_proto::VmDesiredState::Stopped => false,
            controller_proto::VmDesiredState::Unspecified => {
                return Err(Status::invalid_argument(
                    "desired_state must be RUNNING or STOPPED",
                ));
            }
        };
        let state = self
            .set_vm_desired_state_internal(&req.vm_id, &req.target_node, auto_start)
            .await?;
        self.log_replication_event(
            &actor,
            Some("SetVmDesiredState"),
            EVT_VM_DESIRED_STATE_SET,
            &format!("vm/{}", req.vm_id),
            serde_json::json!({
                "vmId": req.vm_id,
                "targetNode": req.target_node,
                "autoStart": auto_start,
            }),
        );

        Ok(Response::new(controller_proto::SetVmDesiredStateResponse {
            state,
        }))
    }

    async fn get_vm(
        &self,
        request: Request<controller_proto::GetVmRequest>,
    ) -> Result<Response<controller_proto::GetVmResponse>, Status> {
        self.require_operator(&request, OperatorRole::ReadOnly)?;
        let req = request.into_inner();
        let node = self.resolve_node_for_vm(&req.vm_id, &req.target_node)?;
        let db_vm = self
            .db
            .get_vm(&req.vm_id)
            .map_err(|e| Status::internal(e.to_string()))?
            .or_else(|| {
                self.db
                    .list_vms_for_node(&node.id)
                    .ok()
                    .and_then(|rows| rows.into_iter().find(|vm| vm.name == req.vm_id))
            })
            .ok_or_else(|| Status::not_found(format!("VM {} not found", req.vm_id)))?;

        let mut client = self
            .ensure_compute_client_for_address(&node.address)
            .await?;

        let resp = client
            .get_vm(node_proto::GetVmRequest {
                vm_id: db_vm.name.clone(),
            })
            .await;

        let inner = match resp {
            Ok(resp) => resp.into_inner(),
            Err(err) => {
                warn!(
                    vm_id = %db_vm.id,
                    vm_name = %db_vm.name,
                    node_id = %node.id,
                    error = %err,
                    "runtime VM lookup failed; returning database-backed VM details"
                );
                let spec = Some(controller_proto::VmSpec {
                    id: db_vm.id.clone(),
                    name: db_vm.name.clone(),
                    cpu: db_vm.cpu,
                    memory_bytes: db_vm.memory_bytes,
                    disks: vec![controller_proto::Disk {
                        name: "boot".to_string(),
                        backend_handle: vm_backend_handle(&db_vm),
                        bus: String::new(),
                        device: String::new(),
                    }],
                    nics: crate::vm_nics::spec_nics(
                        &db_vm.network,
                        &self.db.list_vm_nics(&db_vm.id).unwrap_or_default(),
                    ),
                    storage_backend: db_vm.storage_backend.clone(),
                    storage_size_bytes: db_vm.storage_size_bytes,
                    desired_state: if db_vm.auto_start {
                        controller_proto::VmDesiredState::Running as i32
                    } else {
                        controller_proto::VmDesiredState::Stopped as i32
                    },
                    pci_devices: crate::pci::split_pci_devices(&db_vm.pci_devices),
                    gpus: self
                        .vm_gpu_names(&db_vm.id)?
                        .into_iter()
                        .filter(|name| !name.contains('='))
                        .collect(),
                    vgpus: self
                        .vm_gpu_names(&db_vm.id)?
                        .into_iter()
                        .filter(|name| name.contains('='))
                        .collect(),
                    labels: vm_labels(&db_vm.labels),
                });
                let status = Some(controller_proto::VmStatus {
                    id: db_vm.id.clone(),
                    state: state_fallback_without_runtime(db_vm.auto_start),
                    created_at: None,
                    updated_at: None,
                });
                let guest_reachable = self.guest_reachable_for(&db_vm).await;
                let (
                    ha_protection,
                    restart_priority,
                    replica_node,
                    replica_interval_secs,
                    vm_monitoring,
                ) = self.ha_view(&db_vm);
                return Ok(Response::new(controller_proto::GetVmResponse {
                    spec,
                    status,
                    node_id: node.id,
                    assigned_ip: db_vm.vm_ip.clone(),
                    guest_reachable,
                    ha_protection,
                    restart_priority,
                    replica_node,
                    replica_interval_secs,
                    vm_monitoring,
                }));
            }
        };

        let gpu_names = self.vm_gpu_names(&db_vm.id)?;
        let spec = inner.spec.map(|s| {
            let mut disks: Vec<controller_proto::Disk> = s
                .disks
                .into_iter()
                .map(|d| controller_proto::Disk {
                    name: d.name,
                    backend_handle: d.backend_handle,
                    bus: d.bus,
                    device: d.device,
                })
                .collect();
            if db_vm.storage_backend == "lvm" || db_vm.storage_backend == "zfs" {
                let block_path = vm_backend_handle(&db_vm);
                let single = disks.len() == 1;
                for d in &mut disks {
                    if d.name == "boot" || single {
                        d.backend_handle.clone_from(&block_path);
                    }
                }
            }
            if disks.is_empty() {
                disks.push(controller_proto::Disk {
                    name: "boot".to_string(),
                    backend_handle: vm_backend_handle(&db_vm),
                    bus: String::new(),
                    device: String::new(),
                });
            }

            let mut nics: Vec<controller_proto::Nic> = s
                .nics
                .into_iter()
                .map(|n| controller_proto::Nic {
                    network: {
                        let raw = n.network.trim().to_string();
                        if raw.is_empty()
                            || raw.starts_with("tap-")
                            || raw.starts_with("veth")
                            || raw.contains("[kcore-net:")
                        {
                            db_vm.network.clone()
                        } else {
                            raw
                        }
                    },
                    model: n.model,
                    mac_address: n.mac_address,
                })
                .collect();
            if nics.is_empty() || nics.iter().all(|n| n.network.trim().is_empty()) {
                nics = crate::vm_nics::spec_nics(
                    &db_vm.network,
                    &self.db.list_vm_nics(&db_vm.id).unwrap_or_default(),
                );
            } else if self
                .db
                .list_vm_nics(&db_vm.id)
                .map(|rows| !rows.is_empty())
                .unwrap_or(false)
            {
                let stored = crate::vm_nics::spec_nics(
                    &db_vm.network,
                    &self.db.list_vm_nics(&db_vm.id).unwrap_or_default(),
                );
                nics = stored
                    .into_iter()
                    .enumerate()
                    .map(|(index, mut nic)| {
                        if let Some(runtime) = nics.get(index) {
                            if !runtime.mac_address.is_empty() {
                                nic.mac_address = runtime.mac_address.clone();
                            }
                        }
                        nic
                    })
                    .collect();
            }

            controller_proto::VmSpec {
                id: if s.id.is_empty() {
                    db_vm.id.clone()
                } else {
                    s.id
                },
                name: if s.name.is_empty() {
                    db_vm.name.clone()
                } else {
                    s.name
                },
                cpu: if s.cpu == 0 { db_vm.cpu } else { s.cpu },
                memory_bytes: if s.memory_bytes == 0 {
                    db_vm.memory_bytes
                } else {
                    s.memory_bytes
                },
                disks,
                nics,
                storage_backend: db_vm.storage_backend.clone(),
                storage_size_bytes: db_vm.storage_size_bytes,
                desired_state: if db_vm.auto_start {
                    controller_proto::VmDesiredState::Running as i32
                } else {
                    controller_proto::VmDesiredState::Stopped as i32
                },
                pci_devices: crate::pci::split_pci_devices(&db_vm.pci_devices),
                gpus: gpu_names
                    .iter()
                    .filter(|name| !name.contains('='))
                    .cloned()
                    .collect(),
                vgpus: gpu_names
                    .iter()
                    .filter(|name| name.contains('='))
                    .cloned()
                    .collect(),
                labels: vm_labels(&db_vm.labels),
            }
        });

        let status = inner.status.map(|s| controller_proto::VmStatus {
            id: s.id,
            state: controller_state_from_node_state(s.state),
            created_at: s.created_at,
            updated_at: s.updated_at,
        });

        let guest_reachable = self.guest_reachable_for(&db_vm).await;
        let (ha_protection, restart_priority, replica_node, replica_interval_secs, vm_monitoring) =
            self.ha_view(&db_vm);
        Ok(Response::new(controller_proto::GetVmResponse {
            spec,
            status,
            node_id: node.id,
            assigned_ip: db_vm.vm_ip,
            guest_reachable,
            ha_protection,
            restart_priority,
            replica_node,
            replica_interval_secs,
            vm_monitoring,
        }))
    }

    type AttachVmConsoleStream = Pin<
        Box<dyn Stream<Item = Result<controller_proto::ConsoleMessage, Status>> + Send + 'static>,
    >;

    async fn attach_vm_console(
        &self,
        request: Request<tonic::Streaming<controller_proto::ConsoleMessage>>,
    ) -> Result<Response<Self::AttachVmConsoleStream>, Status> {
        self.require_operator(&request, OperatorRole::VmAdmin)?;
        let actor = Self::audit_actor(&request);
        let mut inbound = request.into_inner();
        let first = inbound
            .message()
            .await?
            .ok_or_else(|| Status::invalid_argument("console stream is empty"))?;
        let (vm_name, node_id, to_node_tx, mut from_node) =
            self.connect_vm_console(&first.vm_name, first.data).await?;

        // Session established: who opened which console, and when (created_at).
        self.record_audit(
            &actor,
            "AttachVmConsole",
            &format!("vm/{vm_name}"),
            serde_json::json!({ "nodeId": node_id }).to_string(),
        );

        let (out_tx, out_rx) =
            mpsc::channel::<Result<controller_proto::ConsoleMessage, Status>>(64);
        let out_tx_node = out_tx.clone();

        tokio::spawn(async move {
            loop {
                match from_node.message().await {
                    Ok(Some(msg)) => {
                        let mapped = controller_proto::ConsoleMessage {
                            vm_name: msg.vm_name,
                            data: msg.data,
                        };
                        if out_tx_node.send(Ok(mapped)).await.is_err() {
                            break;
                        }
                    }
                    Ok(None) => break,
                    Err(e) => {
                        let _ = out_tx_node.send(Err(e)).await;
                        break;
                    }
                }
            }
        });

        tokio::spawn(async move {
            while let Ok(Some(msg)) = inbound.message().await {
                if to_node_tx
                    .send(node_proto::ConsoleMessage {
                        vm_name: String::new(),
                        data: msg.data,
                    })
                    .await
                    .is_err()
                {
                    break;
                }
            }
        });

        Ok(Response::new(
            Box::pin(ReceiverStream::new(out_rx)) as Self::AttachVmConsoleStream
        ))
    }

    type OpenVmConsoleStream = Pin<
        Box<dyn Stream<Item = Result<controller_proto::VmConsoleEvent, Status>> + Send + 'static>,
    >;

    async fn open_vm_console(
        &self,
        request: Request<controller_proto::OpenVmConsoleRequest>,
    ) -> Result<Response<Self::OpenVmConsoleStream>, Status> {
        self.require_operator(&request, OperatorRole::VmAdmin)?;
        let actor = Self::audit_actor(&request);
        let owner = auth::peer_cn(&request).unwrap_or_default();
        let (vm_name, node_id, to_node, mut from_node) = self
            .connect_vm_console(&request.get_ref().vm_name, Vec::new())
            .await?;
        self.record_audit(
            &actor,
            "AttachVmConsole",
            &format!("vm/{vm_name}"),
            serde_json::json!({ "nodeId": node_id, "transport": "grpc-web" }).to_string(),
        );

        let session_id = Uuid::new_v4().to_string();
        self.web_consoles
            .0
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(session_id.clone(), WebConsole { owner, to_node });

        let (out_tx, out_rx) =
            mpsc::channel::<Result<controller_proto::VmConsoleEvent, Status>>(64);
        let _ = out_tx
            .send(Ok(controller_proto::VmConsoleEvent {
                session_id: session_id.clone(),
                data: Vec::new(),
            }))
            .await;
        let sessions = self.web_consoles.clone();
        tokio::spawn(async move {
            loop {
                tokio::select! {
                    msg = from_node.message() => match msg {
                        Ok(Some(msg)) => {
                            let event = controller_proto::VmConsoleEvent {
                                session_id: String::new(),
                                data: msg.data,
                            };
                            if out_tx.send(Ok(event)).await.is_err() {
                                break;
                            }
                        }
                        Ok(None) => break,
                        Err(e) => {
                            let _ = out_tx.send(Err(e)).await;
                            break;
                        }
                    },
                    _ = out_tx.closed() => break,
                }
            }
            sessions
                .0
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .remove(&session_id);
        });

        Ok(Response::new(
            Box::pin(ReceiverStream::new(out_rx)) as Self::OpenVmConsoleStream
        ))
    }

    async fn send_vm_console_input(
        &self,
        request: Request<controller_proto::SendVmConsoleInputRequest>,
    ) -> Result<Response<controller_proto::SendVmConsoleInputResponse>, Status> {
        self.require_operator(&request, OperatorRole::VmAdmin)?;
        let owner = auth::peer_cn(&request).unwrap_or_default();
        let req = request.into_inner();
        if req.data.len() > WEB_CONSOLE_MAX_INPUT {
            return Err(Status::invalid_argument(
                "console input is larger than 64 KiB",
            ));
        }
        let to_node = {
            let sessions = self
                .web_consoles
                .0
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            match sessions.get(&req.session_id) {
                Some(session) if session.owner == owner => session.to_node.clone(),
                _ => return Err(Status::not_found("console session not found")),
            }
        };
        tokio::time::timeout(
            Duration::from_secs(10),
            to_node.send(node_proto::ConsoleMessage {
                vm_name: String::new(),
                data: req.data,
            }),
        )
        .await
        .map_err(|_| Status::unavailable("console input is backed up"))?
        .map_err(|_| Status::not_found("console session ended"))?;
        Ok(Response::new(
            controller_proto::SendVmConsoleInputResponse {},
        ))
    }

    async fn list_vms(
        &self,
        request: Request<controller_proto::ListVmsRequest>,
    ) -> Result<Response<controller_proto::ListVmsResponse>, Status> {
        self.require_operator(&request, OperatorRole::ReadOnly)?;
        let req = request.into_inner();

        let rows = if !req.target_node.is_empty() {
            let node = self
                .db
                .get_node_by_address(&req.target_node)
                .map_err(|e| Status::internal(e.to_string()))?
                .or_else(|| self.db.get_node(&req.target_node).ok().flatten())
                .ok_or_else(|| Status::not_found(format!("node {} not found", req.target_node)))?;
            self.db
                .list_vms_for_node(&node.id)
                .map_err(|e| Status::internal(e.to_string()))?
        } else {
            self.db
                .list_vms()
                .map_err(|e| Status::internal(e.to_string()))?
        };

        let node_address_by_id = self
            .db
            .list_nodes()
            .map_err(|e| Status::internal(e.to_string()))?
            .into_iter()
            .map(|n| (n.id, n.address))
            .collect::<std::collections::HashMap<_, _>>();

        // Dial missing nodes together. Sequential connects used to add a full
        // TCP timeout for every unreachable host before the page could render.
        let mut missing_addresses = Vec::new();
        let mut seen_addresses = HashSet::new();
        for vm in &rows {
            if let Some(node_address) = node_address_by_id.get(&vm.node_id) {
                if self.clients.get_compute(node_address).is_none()
                    && seen_addresses.insert(node_address.clone())
                {
                    missing_addresses.push(node_address.clone());
                }
            }
        }
        let mut connects = tokio::task::JoinSet::new();
        for address in missing_addresses {
            let clients = self.clients.clone();
            connects.spawn(async move {
                if let Err(err) = clients.connect(&address).await {
                    warn!(address = %address, error = %err, "failed to refresh node compute client");
                }
            });
        }
        while connects.join_next().await.is_some() {}

        let vm_count = rows.len();
        let mut fallback_states: Vec<i32> = Vec::with_capacity(vm_count);
        let mut set = tokio::task::JoinSet::new();

        for (idx, vm) in rows.iter().enumerate() {
            fallback_states.push(state_fallback_without_runtime(vm.auto_start));
            if let Some(node_address) = node_address_by_id.get(&vm.node_id) {
                if let Some(mut compute) = self.clients.get_compute(node_address) {
                    let vm_name = vm.name.clone();
                    let node_id = vm.node_id.clone();
                    let addr = node_address.clone();
                    set.spawn(async move {
                        let result = tokio::time::timeout(
                            Duration::from_secs(3),
                            compute.get_vm(node_proto::GetVmRequest {
                                vm_id: vm_name.clone(),
                            }),
                        )
                        .await;
                        (idx, vm_name, node_id, addr, result)
                    });
                }
            }
        }

        let mut live_states: Vec<Option<i32>> = vec![None; vm_count];
        while let Some(Ok((idx, vm_name, node_id, addr, result))) = set.join_next().await {
            match result {
                Ok(Ok(resp)) => {
                    if let Some(status) = resp.into_inner().status {
                        live_states[idx] = Some(controller_state_from_node_state(status.state));
                    }
                }
                Ok(Err(err)) => {
                    warn!(node_id = %node_id, vm_name = %vm_name, address = %addr, error = %err, "failed to fetch runtime VM state");
                }
                Err(_) => {
                    warn!(node_id = %node_id, vm_name = %vm_name, address = %addr, "timed out fetching runtime VM state");
                }
            }
        }

        let ha_rows = self.db.list_vm_ha().unwrap_or_default();
        let infos: Vec<_> = rows
            .into_iter()
            .enumerate()
            .map(|(i, vm)| {
                let state = live_states[i].unwrap_or(fallback_states[i]);
                let policy = ha_rows.iter().find(|row| row.vm_id == vm.id);
                let replica = policy.map(|row| row.replica_node.as_str()).unwrap_or("");
                controller_proto::VmInfo {
                    id: vm.id,
                    name: vm.name,
                    state,
                    cpu: vm.cpu,
                    memory_bytes: vm.memory_bytes,
                    node_id: vm.node_id,
                    created_at: None,
                    storage_backend: vm.storage_backend.clone(),
                    storage_size_bytes: vm.storage_size_bytes,
                    labels: vm_labels(&vm.labels),
                    ha_protection: crate::ha::protection(&vm.storage_backend, replica).to_string(),
                    restart_priority: policy
                        .map(|row| row.restart_priority.clone())
                        .unwrap_or_else(|| crate::ha::PRIORITY_MEDIUM.to_string()),
                    replica_node: replica.to_string(),
                    replica_interval_secs: policy
                        .map(|row| i32::try_from(row.replica_interval_secs).unwrap_or(0))
                        .unwrap_or(0),
                    guest_heartbeat_unix: policy.map(|row| row.last_guest_heartbeat).unwrap_or(0),
                }
            })
            .collect();

        Ok(Response::new(controller_proto::ListVmsResponse {
            vms: infos,
        }))
    }

    async fn create_workload(
        &self,
        request: Request<controller_proto::CreateWorkloadRequest>,
    ) -> Result<Response<controller_proto::CreateWorkloadResponse>, Status> {
        self.require_operator(&request, OperatorRole::VmAdmin)?;
        let actor = Self::audit_actor(&request);
        let _audit_guard = Self::push_audit_actor(&actor);
        let req = request.into_inner();
        let kind = controller_proto::WorkloadKind::try_from(req.kind)
            .unwrap_or(controller_proto::WorkloadKind::Unspecified);
        match kind {
            controller_proto::WorkloadKind::Vm => {
                let vm_spec = req.vm_spec.ok_or_else(|| {
                    Status::invalid_argument("vm_spec is required for VM workload")
                })?;
                let vm_resp = self
                    .create_vm(Request::new(controller_proto::CreateVmRequest {
                        target_node: req.target_node.clone(),
                        spec: Some(vm_spec),
                        image_url: req.image_url.clone(),
                        image_sha256: req.image_sha256.clone(),
                        cloud_init_user_data: req.cloud_init_user_data.clone(),
                        image_path: req.image_path.clone(),
                        image_format: req.image_format.clone(),
                        ssh_key_names: req.ssh_key_names.clone(),
                        storage_backend: req.storage_backend,
                        storage_size_bytes: req.storage_size_bytes,
                        target_dc: String::new(),
                        node_labels: Vec::new(),
                        anti_affinity: String::new(),
                        ..Default::default()
                    }))
                    .await?
                    .into_inner();

                if let Some(vm_row) = self.db.get_vm(&vm_resp.vm_id).map_err(|e| {
                    Status::internal(format!("fetching vm after create workload: {e}"))
                })? {
                    let _ = self.db.upsert_workload(&WorkloadRow {
                        id: vm_row.id.clone(),
                        name: vm_row.name.clone(),
                        kind: "vm".to_string(),
                        node_id: vm_row.node_id.clone(),
                        runtime_state: vm_row.runtime_state.clone(),
                        desired_state: if vm_row.auto_start {
                            "running".to_string()
                        } else {
                            "stopped".to_string()
                        },
                        vm_id: vm_row.id.clone(),
                        container_image: String::new(),
                        network: vm_row.network.clone(),
                        storage_backend: vm_row.storage_backend.clone(),
                        storage_size_bytes: vm_row.storage_size_bytes,
                        created_at: String::new(),
                    });
                }

                Ok(Response::new(controller_proto::CreateWorkloadResponse {
                    kind: controller_proto::WorkloadKind::Vm as i32,
                    workload_id: vm_resp.vm_id,
                    node_id: vm_resp.node_id,
                    vm_state: vm_resp.state,
                    container_state: controller_proto::ContainerState::Unknown as i32,
                    action: vm_resp.action,
                    changed_fields: vm_resp.changed_fields,
                }))
            }
            controller_proto::WorkloadKind::Container => {
                let spec = req.container_spec.ok_or_else(|| {
                    Status::invalid_argument("container_spec is required for container workload")
                })?;
                if spec.name.trim().is_empty() || spec.image.trim().is_empty() {
                    return Err(Status::invalid_argument(
                        "container_spec.name and container_spec.image are required",
                    ));
                }

                // Upsert: if a container workload with this name already exists,
                // diff it and apply desired_state changes (or reject immutable
                // field changes).
                if let Some(existing) = self
                    .db
                    .get_workload(spec.name.trim())
                    .map_err(|e| Status::internal(format!("fetching workload: {e}")))?
                    .filter(|w| w.kind == "container")
                {
                    let storage_backend_str = if req.storage_backend == 0 {
                        String::new()
                    } else {
                        normalize_storage_backend(req.storage_backend, false)?
                    };
                    let extras = crate::grpc::diff::StoredContainerExtras {
                        image: existing.container_image.clone(),
                        command: Vec::new(),
                        env: std::collections::HashMap::new(),
                        ports: Vec::new(),
                        mount_target: String::new(),
                    };
                    let apply = crate::grpc::diff::ContainerApply {
                        spec: &spec,
                        storage_backend: &storage_backend_str,
                        storage_size_bytes: req.storage_size_bytes,
                    };
                    let d = crate::grpc::diff::diff_container(&existing, &extras, &apply);

                    if !d.immutable.is_empty() {
                        return Err(Status::invalid_argument(format!(
                            "cannot change immutable field(s) on container '{}': {} \
                             (delete the container and recreate)",
                            existing.name,
                            d.immutable.join(", ")
                        )));
                    }

                    let current_state_enum = match existing.runtime_state.as_str() {
                        "running" => controller_proto::ContainerState::Running as i32,
                        "stopped" => controller_proto::ContainerState::Stopped as i32,
                        "created" => controller_proto::ContainerState::Created as i32,
                        "error" => controller_proto::ContainerState::Error as i32,
                        _ => controller_proto::ContainerState::Unknown as i32,
                    };

                    if d.mutable.is_empty() {
                        return Ok(Response::new(controller_proto::CreateWorkloadResponse {
                            kind: controller_proto::WorkloadKind::Container as i32,
                            workload_id: existing.id,
                            node_id: existing.node_id,
                            vm_state: controller_proto::VmState::Unknown as i32,
                            container_state: current_state_enum,
                            action: controller_proto::ApplyAction::Unchanged as i32,
                            changed_fields: Vec::new(),
                        }));
                    }

                    let mut changed_fields: Vec<String> = Vec::new();
                    let mut final_state_enum = current_state_enum;
                    if d.mutable.iter().any(|f| f == "desired_state") {
                        let want_running = matches!(
                            controller_proto::WorkloadDesiredState::try_from(spec.desired_state)
                                .unwrap_or(controller_proto::WorkloadDesiredState::Unspecified),
                            controller_proto::WorkloadDesiredState::Running,
                        );
                        let resp = self
                            .set_workload_desired_state(Request::new(
                                controller_proto::SetWorkloadDesiredStateRequest {
                                    kind: controller_proto::WorkloadKind::Container as i32,
                                    workload_id: existing.id.clone(),
                                    desired_state: if want_running {
                                        controller_proto::WorkloadDesiredState::Running as i32
                                    } else {
                                        controller_proto::WorkloadDesiredState::Stopped as i32
                                    },
                                    target_node: String::new(),
                                },
                            ))
                            .await?
                            .into_inner();
                        final_state_enum = resp.container_state;
                        changed_fields.push("desired_state".into());
                    }

                    return Ok(Response::new(controller_proto::CreateWorkloadResponse {
                        kind: controller_proto::WorkloadKind::Container as i32,
                        workload_id: existing.id,
                        node_id: existing.node_id,
                        vm_state: controller_proto::VmState::Unknown as i32,
                        container_state: final_state_enum,
                        action: controller_proto::ApplyAction::Updated as i32,
                        changed_fields,
                    }));
                }

                let node = if !req.target_node.is_empty() {
                    self.db
                        .get_node_by_address(&req.target_node)
                        .map_err(|e| Status::internal(e.to_string()))?
                        .or_else(|| self.db.get_node(&req.target_node).ok().flatten())
                        .ok_or_else(|| {
                            Status::not_found(format!("node {} not found", req.target_node))
                        })?
                } else {
                    let nodes = self
                        .db
                        .list_nodes()
                        .map_err(|e| Status::internal(e.to_string()))?;
                    scheduler::select_node(&nodes)
                        .cloned()
                        .ok_or_else(|| Status::unavailable("no ready nodes"))?
                };

                let mut container = self
                    .ensure_container_client_for_address(&node.address)
                    .await?;
                let created = container
                    .create_container(node_proto::CreateContainerRequest {
                        spec: Some(node_proto::ContainerSpec {
                            name: spec.name.clone(),
                            image: spec.image.clone(),
                            network: spec.network.clone(),
                            command: spec.command.clone(),
                            env: spec.env.clone(),
                            ports: spec.ports.clone(),
                            storage_backend: spec.storage_backend.clone(),
                            storage_size_bytes: spec.storage_size_bytes,
                            mount_target: spec.mount_target.clone(),
                        }),
                    })
                    .await
                    .map_err(|e| Status::internal(format!("creating container on node: {e}")))?
                    .into_inner()
                    .container
                    .ok_or_else(|| Status::internal("missing container response from node"))?;

                let workload_id = if created.id.is_empty() {
                    format!("ctr-{}", Uuid::new_v4())
                } else {
                    created.id.clone()
                };

                // Honor declarative desired_state; default to running.
                let desired_running = !matches!(
                    controller_proto::WorkloadDesiredState::try_from(spec.desired_state)
                        .unwrap_or(controller_proto::WorkloadDesiredState::Unspecified),
                    controller_proto::WorkloadDesiredState::Stopped,
                );

                // For declarative desired_state=stopped: try to stop the
                // container BEFORE persisting the row so that the persisted
                // runtime_state and the response state never claim "stopped"
                // while the container is actually still running on the node.
                // The desired_state is recorded unconditionally so the
                // reconciler keeps trying if the immediate stop attempt fails.
                let mut runtime_state_str = "running".to_string();
                let mut final_state = created.state;
                if !desired_running {
                    let mut stop_client = container.clone();
                    match stop_client
                        .stop_container(node_proto::StopContainerRequest {
                            name: created.name.clone(),
                        })
                        .await
                    {
                        Ok(_) => {
                            runtime_state_str = "stopped".to_string();
                            final_state = controller_proto::ContainerState::Stopped as i32;
                        }
                        Err(e) => {
                            warn!(
                                name = %created.name,
                                node_id = %node.id,
                                error = %e,
                                "failed to stop container after create for desired_state=stopped; reconciler will retry"
                            );
                        }
                    }
                }

                self.db
                    .upsert_workload(&WorkloadRow {
                        id: workload_id.clone(),
                        name: created.name.clone(),
                        kind: "container".to_string(),
                        node_id: node.id.clone(),
                        runtime_state: runtime_state_str,
                        desired_state: if desired_running {
                            "running".to_string()
                        } else {
                            "stopped".to_string()
                        },
                        vm_id: String::new(),
                        container_image: created.image.clone(),
                        network: spec.network.clone(),
                        storage_backend: normalize_storage_backend(req.storage_backend, false)?,
                        storage_size_bytes: req.storage_size_bytes.max(0),
                        created_at: String::new(),
                    })
                    .map_err(|e| Status::internal(format!("storing workload row: {e}")))?;

                self.record_audit(
                    &actor,
                    "CreateWorkload",
                    &format!("workload/{workload_id}"),
                    "",
                );

                Ok(Response::new(controller_proto::CreateWorkloadResponse {
                    kind: controller_proto::WorkloadKind::Container as i32,
                    workload_id,
                    node_id: node.id,
                    vm_state: controller_proto::VmState::Unknown as i32,
                    container_state: final_state,
                    action: controller_proto::ApplyAction::Created as i32,
                    changed_fields: Vec::new(),
                }))
            }
            controller_proto::WorkloadKind::Unspecified => {
                Err(Status::invalid_argument("kind is required"))
            }
        }
    }

    async fn delete_workload(
        &self,
        request: Request<controller_proto::DeleteWorkloadRequest>,
    ) -> Result<Response<controller_proto::DeleteWorkloadResponse>, Status> {
        self.require_operator(&request, OperatorRole::VmAdmin)?;
        let actor = Self::audit_actor(&request);
        let _audit_guard = Self::push_audit_actor(&actor);
        let req = request.into_inner();
        let kind = controller_proto::WorkloadKind::try_from(req.kind)
            .unwrap_or(controller_proto::WorkloadKind::Unspecified);
        match kind {
            controller_proto::WorkloadKind::Vm => {
                let _ = self
                    .delete_vm(Request::new(controller_proto::DeleteVmRequest {
                        vm_id: req.workload_id.clone(),
                        target_node: req.target_node.clone(),
                        delete_data_volumes: false,
                    }))
                    .await?;
                let _ = self.db.delete_workload_by_id_or_name(&req.workload_id);
                Ok(Response::new(controller_proto::DeleteWorkloadResponse {
                    success: true,
                }))
            }
            controller_proto::WorkloadKind::Container => {
                let wl = self
                    .db
                    .get_workload(&req.workload_id)
                    .map_err(|e| Status::internal(format!("fetching workload: {e}")))?;
                let node = if let Some(wl) = &wl {
                    self.db
                        .get_node(&wl.node_id)
                        .map_err(|e| Status::internal(e.to_string()))?
                        .ok_or_else(|| {
                            Status::not_found(format!("node {} not found", wl.node_id))
                        })?
                } else if !req.target_node.is_empty() {
                    self.db
                        .get_node_by_address(&req.target_node)
                        .map_err(|e| Status::internal(e.to_string()))?
                        .or_else(|| self.db.get_node(&req.target_node).ok().flatten())
                        .ok_or_else(|| {
                            Status::not_found(format!("node {} not found", req.target_node))
                        })?
                } else {
                    return Err(Status::not_found(format!(
                        "workload {} not found",
                        req.workload_id
                    )));
                };
                let name = wl
                    .as_ref()
                    .map(|w| w.name.clone())
                    .unwrap_or_else(|| req.workload_id.clone());
                let mut container = self
                    .ensure_container_client_for_address(&node.address)
                    .await?;
                let _ = container
                    .delete_container(node_proto::DeleteContainerRequest { name, force: true })
                    .await
                    .map_err(|e| Status::internal(format!("deleting container on node: {e}")))?;
                let _ = self.db.delete_workload_by_id_or_name(&req.workload_id);
                self.record_audit(
                    &actor,
                    "DeleteWorkload",
                    &format!("workload/{}", req.workload_id),
                    "",
                );
                Ok(Response::new(controller_proto::DeleteWorkloadResponse {
                    success: true,
                }))
            }
            controller_proto::WorkloadKind::Unspecified => {
                Err(Status::invalid_argument("kind is required"))
            }
        }
    }

    async fn set_workload_desired_state(
        &self,
        request: Request<controller_proto::SetWorkloadDesiredStateRequest>,
    ) -> Result<Response<controller_proto::SetWorkloadDesiredStateResponse>, Status> {
        self.require_operator(&request, OperatorRole::VmAdmin)?;
        let actor = Self::audit_actor(&request);
        let _audit_guard = Self::push_audit_actor(&actor);
        let req = request.into_inner();
        let desired = controller_proto::WorkloadDesiredState::try_from(req.desired_state)
            .unwrap_or(controller_proto::WorkloadDesiredState::Unspecified);
        let kind = controller_proto::WorkloadKind::try_from(req.kind)
            .unwrap_or(controller_proto::WorkloadKind::Unspecified);
        match kind {
            controller_proto::WorkloadKind::Vm => {
                let vm_desired = match desired {
                    controller_proto::WorkloadDesiredState::Running => {
                        controller_proto::VmDesiredState::Running as i32
                    }
                    controller_proto::WorkloadDesiredState::Stopped => {
                        controller_proto::VmDesiredState::Stopped as i32
                    }
                    controller_proto::WorkloadDesiredState::Unspecified => {
                        return Err(Status::invalid_argument("desired_state is required"));
                    }
                };
                let resp = self
                    .set_vm_desired_state(Request::new(
                        controller_proto::SetVmDesiredStateRequest {
                            vm_id: req.workload_id.clone(),
                            desired_state: vm_desired,
                            target_node: req.target_node.clone(),
                        },
                    ))
                    .await?
                    .into_inner();
                let _ = self.db.update_workload_desired_state(
                    &req.workload_id,
                    if desired == controller_proto::WorkloadDesiredState::Running {
                        "running"
                    } else {
                        "stopped"
                    },
                );
                Ok(Response::new(
                    controller_proto::SetWorkloadDesiredStateResponse {
                        kind: controller_proto::WorkloadKind::Vm as i32,
                        vm_state: resp.state,
                        container_state: controller_proto::ContainerState::Unknown as i32,
                    },
                ))
            }
            controller_proto::WorkloadKind::Container => {
                let wl = self
                    .db
                    .get_workload(&req.workload_id)
                    .map_err(|e| Status::internal(format!("fetching workload: {e}")))?
                    .ok_or_else(|| {
                        Status::not_found(format!("workload {} not found", req.workload_id))
                    })?;
                let node = self
                    .db
                    .get_node(&wl.node_id)
                    .map_err(|e| Status::internal(e.to_string()))?
                    .ok_or_else(|| Status::not_found(format!("node {} not found", wl.node_id)))?;
                let mut container = self
                    .ensure_container_client_for_address(&node.address)
                    .await?;
                let state = match desired {
                    controller_proto::WorkloadDesiredState::Running => {
                        let resp = container
                            .start_container(node_proto::StartContainerRequest {
                                name: wl.name.clone(),
                            })
                            .await
                            .map_err(|e| Status::internal(format!("starting container: {e}")))?
                            .into_inner();
                        let _ = self.db.update_workload_runtime_state(&wl.id, "running");
                        let _ = self.db.update_workload_desired_state(&wl.id, "running");
                        resp.container
                            .map(|c| c.state)
                            .unwrap_or(controller_proto::ContainerState::Running as i32)
                    }
                    controller_proto::WorkloadDesiredState::Stopped => {
                        let resp = container
                            .stop_container(node_proto::StopContainerRequest {
                                name: wl.name.clone(),
                            })
                            .await
                            .map_err(|e| Status::internal(format!("stopping container: {e}")))?
                            .into_inner();
                        let _ = self.db.update_workload_runtime_state(&wl.id, "stopped");
                        let _ = self.db.update_workload_desired_state(&wl.id, "stopped");
                        resp.container
                            .map(|c| c.state)
                            .unwrap_or(controller_proto::ContainerState::Stopped as i32)
                    }
                    controller_proto::WorkloadDesiredState::Unspecified => {
                        return Err(Status::invalid_argument("desired_state is required"));
                    }
                };
                self.record_audit(
                    &actor,
                    "SetWorkloadDesiredState",
                    &format!("workload/{}", req.workload_id),
                    "",
                );
                Ok(Response::new(
                    controller_proto::SetWorkloadDesiredStateResponse {
                        kind: controller_proto::WorkloadKind::Container as i32,
                        vm_state: controller_proto::VmState::Unknown as i32,
                        container_state: state,
                    },
                ))
            }
            controller_proto::WorkloadKind::Unspecified => {
                Err(Status::invalid_argument("kind is required"))
            }
        }
    }

    async fn get_workload(
        &self,
        request: Request<controller_proto::GetWorkloadRequest>,
    ) -> Result<Response<controller_proto::GetWorkloadResponse>, Status> {
        self.require_operator(&request, OperatorRole::ReadOnly)?;
        let req = request.into_inner();
        let kind = controller_proto::WorkloadKind::try_from(req.kind)
            .unwrap_or(controller_proto::WorkloadKind::Unspecified);
        match kind {
            controller_proto::WorkloadKind::Vm => {
                let vm = self
                    .get_vm(Request::new(controller_proto::GetVmRequest {
                        vm_id: req.workload_id,
                        target_node: req.target_node,
                    }))
                    .await?
                    .into_inner();
                Ok(Response::new(controller_proto::GetWorkloadResponse {
                    kind: controller_proto::WorkloadKind::Vm as i32,
                    vm_spec: vm.spec,
                    container_spec: None,
                    vm_status: vm.status,
                    container_info: None,
                    node_id: vm.node_id,
                    assigned_ip: vm.assigned_ip,
                }))
            }
            controller_proto::WorkloadKind::Container => {
                let wl = self
                    .db
                    .get_workload(&req.workload_id)
                    .map_err(|e| Status::internal(format!("fetching workload: {e}")))?
                    .ok_or_else(|| {
                        Status::not_found(format!("workload {} not found", req.workload_id))
                    })?;
                let node = self
                    .db
                    .get_node(&wl.node_id)
                    .map_err(|e| Status::internal(e.to_string()))?
                    .ok_or_else(|| Status::not_found(format!("node {} not found", wl.node_id)))?;
                let mut container = self
                    .ensure_container_client_for_address(&node.address)
                    .await?;
                let info = container
                    .get_container(node_proto::GetContainerRequest {
                        name: wl.name.clone(),
                    })
                    .await
                    .map_err(|e| Status::internal(format!("getting container: {e}")))?
                    .into_inner()
                    .container;
                let c = info.ok_or_else(|| Status::not_found("container not found on node"))?;
                Ok(Response::new(controller_proto::GetWorkloadResponse {
                    kind: controller_proto::WorkloadKind::Container as i32,
                    vm_spec: None,
                    container_spec: Some(controller_proto::ContainerSpec {
                        name: c.name.clone(),
                        image: c.image.clone(),
                        network: wl.network,
                        command: Vec::new(),
                        env: std::collections::HashMap::new(),
                        ports: Vec::new(),
                        storage_backend: wl.storage_backend.clone(),
                        storage_size_bytes: wl.storage_size_bytes,
                        mount_target: "/data".to_string(),
                        desired_state: match wl.desired_state.as_str() {
                            "running" => controller_proto::WorkloadDesiredState::Running as i32,
                            "stopped" => controller_proto::WorkloadDesiredState::Stopped as i32,
                            _ => controller_proto::WorkloadDesiredState::Unspecified as i32,
                        },
                    }),
                    vm_status: None,
                    container_info: Some(controller_proto::ContainerInfo {
                        id: c.id,
                        name: c.name,
                        image: c.image,
                        state: c.state,
                        status: c.status,
                        node_id: node.id.clone(),
                        created_at: None,
                    }),
                    node_id: node.id.clone(),
                    assigned_ip: String::new(),
                }))
            }
            controller_proto::WorkloadKind::Unspecified => {
                Err(Status::invalid_argument("kind is required"))
            }
        }
    }

    async fn list_workloads(
        &self,
        request: Request<controller_proto::ListWorkloadsRequest>,
    ) -> Result<Response<controller_proto::ListWorkloadsResponse>, Status> {
        self.require_operator(&request, OperatorRole::ReadOnly)?;
        let req = request.into_inner();
        let kind = controller_proto::WorkloadKind::try_from(req.kind)
            .unwrap_or(controller_proto::WorkloadKind::Unspecified);

        let mut vms = Vec::new();
        let mut containers = Vec::new();

        if kind == controller_proto::WorkloadKind::Unspecified
            || kind == controller_proto::WorkloadKind::Vm
        {
            vms = self
                .list_vms(Request::new(controller_proto::ListVmsRequest {
                    target_node: req.target_node.clone(),
                }))
                .await?
                .into_inner()
                .vms;
        }

        if kind == controller_proto::WorkloadKind::Unspecified
            || kind == controller_proto::WorkloadKind::Container
        {
            let node_filter = if req.target_node.trim().is_empty() {
                None
            } else {
                self.db
                    .get_node_by_address(&req.target_node)
                    .map_err(|e| Status::internal(e.to_string()))?
                    .or_else(|| self.db.get_node(&req.target_node).ok().flatten())
                    .map(|n| n.id)
            };
            let rows = self
                .db
                .list_workloads(Some("container"), node_filter.as_deref())
                .map_err(|e| Status::internal(format!("listing container workloads: {e}")))?;
            containers = rows
                .into_iter()
                .map(|w| controller_proto::ContainerInfo {
                    id: w.id,
                    name: w.name,
                    image: w.container_image,
                    state: match w.runtime_state.as_str() {
                        "running" => controller_proto::ContainerState::Running as i32,
                        "stopped" => controller_proto::ContainerState::Stopped as i32,
                        "created" => controller_proto::ContainerState::Created as i32,
                        "error" => controller_proto::ContainerState::Error as i32,
                        _ => controller_proto::ContainerState::Unknown as i32,
                    },
                    status: w.runtime_state,
                    node_id: w.node_id,
                    created_at: parse_datetime_to_timestamp(&w.created_at),
                })
                .collect();
        }

        Ok(Response::new(controller_proto::ListWorkloadsResponse {
            vms,
            containers,
        }))
    }

    async fn create_network(
        &self,
        request: Request<controller_proto::CreateNetworkRequest>,
    ) -> Result<Response<controller_proto::CreateNetworkResponse>, Status> {
        self.require_operator(&request, OperatorRole::VmAdmin)?;
        let actor = Self::audit_actor(&request);
        let req = request.into_inner();
        let name = validate_network_name(&req.name)?;
        let policy = explicit_net_policy(&req)?;
        let external_ip = validate_ipv4(&req.external_ip, "external_ip")?;
        let gateway_ip = validate_ipv4(&req.gateway_ip, "gateway_ip")?;
        let internal_netmask = if req.internal_netmask.trim().is_empty() {
            "255.255.255.0".to_string()
        } else {
            validate_netmask(&req.internal_netmask)?
        };

        let node = if !req.target_node.is_empty() {
            self.db
                .get_node_by_address(&req.target_node)
                .map_err(|e| Status::internal(e.to_string()))?
                .or_else(|| self.db.get_node(&req.target_node).ok().flatten())
                .ok_or_else(|| Status::not_found(format!("node {} not found", req.target_node)))?
        } else {
            let nodes = self
                .db
                .list_nodes()
                .map_err(|e| Status::internal(e.to_string()))?;
            scheduler::select_node(&nodes)
                .cloned()
                .ok_or_else(|| Status::unavailable("no ready nodes"))?
        };

        let network_type = validate_network_type(&req.network_type)?;

        // Upsert: if the network already exists on this node, the request is
        // idempotent: equal spec → UNCHANGED; any other change is rejected
        // because every network field is immutable in v1.
        if let Some(existing) = self
            .db
            .get_network_for_node(&node.id, &name)
            .map_err(|e| Status::internal(format!("checking existing network: {e}")))?
        {
            let enable_outbound_nat_expected = match network_type.as_str() {
                "bridge" => false,
                "nat" => true,
                "vxlan" => req.enable_outbound_nat,
                _ => true,
            };
            let apply = crate::grpc::diff::NetworkApply {
                external_ip: req.external_ip.trim(),
                gateway_ip: req.gateway_ip.trim(),
                internal_netmask: if req.internal_netmask.trim().is_empty() {
                    "255.255.255.0"
                } else {
                    req.internal_netmask.trim()
                },
                allowed_tcp_ports: req.allowed_tcp_ports.clone(),
                allowed_udp_ports: req.allowed_udp_ports.clone(),
                vlan_id: req.vlan_id,
                network_type: &network_type,
                enable_outbound_nat: enable_outbound_nat_expected,
            };
            let diff = crate::grpc::diff::diff_network(&existing, &apply);
            if !diff.immutable.is_empty() {
                return Err(Status::invalid_argument(format!(
                    "cannot change immutable field(s) on network '{}' on node '{}': {} \
                     (delete the network and recreate)",
                    name,
                    node.id,
                    diff.immutable.join(", ")
                )));
            }
            if let Some((prefix, gateway, east_west)) = &policy {
                let stored = self
                    .db
                    .get_network_policy(&node.id, &name)
                    .map_err(|e| Status::internal(format!("reading network policy: {e}")))?;
                if &stored.ipv6_prefix != prefix
                    || &stored.ipv6_gateway != gateway
                    || stored.east_west != *east_west
                {
                    return Err(Status::invalid_argument(format!(
                        "cannot change immutable field(s) on network '{name}' on node '{}': \
                         ipv6Prefix, ipv6Gateway, eastWestFirewall \
                         (delete the network and recreate)",
                        node.id
                    )));
                }
            }
            return Ok(Response::new(controller_proto::CreateNetworkResponse {
                success: true,
                message: format!("network '{name}' on node '{}' unchanged", node.id),
                node_id: node.id,
                action: controller_proto::ApplyAction::Unchanged as i32,
                changed_fields: Vec::new(),
            }));
        }

        if network_type == "bridge" && req.vlan_id == 0 {
            return Err(Status::failed_precondition(
                "bridge-mode networks without a VLAN ID enslave the management NIC, \
                 which severs host connectivity. Use --vlan-id to create a VLAN \
                 sub-interface, or use 'nat' or 'vxlan' network types instead."
                    .to_string(),
            ));
        }

        if network_type == "vxlan" && node.disable_vxlan {
            return Err(Status::failed_precondition(format!(
                "VXLAN is disabled on node '{}'; cannot create vxlan network",
                node.id
            )));
        }

        let enable_outbound_nat = match network_type.as_str() {
            "bridge" => false,
            "nat" => true,
            "vxlan" => req.enable_outbound_nat,
            _ => true,
        };

        let vni = if network_type == "vxlan" {
            compute_vni(&name)
        } else {
            0
        };

        self.db
            .insert_network(&NetworkRow {
                name: name.clone(),
                external_ip,
                gateway_ip,
                internal_netmask,
                node_id: node.id.clone(),
                allowed_tcp_ports: req
                    .allowed_tcp_ports
                    .iter()
                    .map(|p| p.to_string())
                    .collect::<Vec<_>>()
                    .join(","),
                allowed_udp_ports: req
                    .allowed_udp_ports
                    .iter()
                    .map(|p| p.to_string())
                    .collect::<Vec<_>>()
                    .join(","),
                vlan_id: req.vlan_id,
                network_type: network_type.clone(),
                enable_outbound_nat,
                vni,
                next_ip: 2,
            })
            .map_err(|e| Status::internal(format!("storing network: {e}")))?;

        if let Some((prefix, gateway, east_west)) = &policy {
            self.db
                .upsert_network_policy(&crate::db::NetworkPolicyRow {
                    node_id: node.id.clone(),
                    name: name.clone(),
                    east_west: *east_west,
                    ipv6_prefix: prefix.clone(),
                    ipv6_gateway: gateway.clone(),
                    ipv6_next: 2,
                })
                .map_err(|e| Status::internal(format!("storing network policy: {e}")))?;
        }

        self.push_config_to_node(&node).await?;

        if network_type == "vxlan" {
            self.refresh_vxlan_peers(&name, &node.id).await;
        }

        self.log_replication_event(
            &actor,
            Some("CreateNetwork"),
            EVT_NETWORK_CREATE,
            &format!("network/{}/{}", node.id, name),
            serde_json::json!({
                "name": name,
                "nodeId": node.id,
                "externalIp": req.external_ip,
                "gatewayIp": req.gateway_ip,
                "internalNetmask": req.internal_netmask,
                "allowedTcpPorts": req.allowed_tcp_ports,
                "allowedUdpPorts": req.allowed_udp_ports,
                "networkType": network_type,
                "vlanId": req.vlan_id,
                "enableOutboundNat": enable_outbound_nat,
                "vni": vni,
                "nextIp": 2,
                "ipv6Prefix": policy.as_ref().map(|(prefix, _, _)| prefix.clone()).unwrap_or_default(),
                "ipv6Gateway": policy.as_ref().map(|(_, gateway, _)| gateway.clone()).unwrap_or_default(),
                "eastWestFirewall": policy.as_ref().map(|(_, _, east)| *east).unwrap_or(false),
            }),
        );

        Ok(Response::new(controller_proto::CreateNetworkResponse {
            success: true,
            message: format!("created network '{name}' on node '{}'", node.id),
            node_id: node.id,
            action: controller_proto::ApplyAction::Created as i32,
            changed_fields: Vec::new(),
        }))
    }

    async fn delete_network(
        &self,
        request: Request<controller_proto::DeleteNetworkRequest>,
    ) -> Result<Response<controller_proto::DeleteNetworkResponse>, Status> {
        self.require_operator(&request, OperatorRole::VmAdmin)?;
        let actor = Self::audit_actor(&request);
        let req = request.into_inner();
        let name = req.name.trim();
        if name.is_empty() {
            return Err(Status::invalid_argument("network name is required"));
        }
        if name == "default" {
            return Err(Status::invalid_argument(
                "cannot delete reserved network 'default'",
            ));
        }

        let node = if !req.target_node.is_empty() {
            self.db
                .get_node_by_address(&req.target_node)
                .map_err(|e| Status::internal(e.to_string()))?
                .or_else(|| self.db.get_node(&req.target_node).ok().flatten())
                .ok_or_else(|| Status::not_found(format!("node {} not found", req.target_node)))?
        } else {
            let matches = self
                .db
                .list_networks()
                .map_err(|e| Status::internal(format!("listing networks: {e}")))?
                .into_iter()
                .filter(|n| n.name == name)
                .collect::<Vec<_>>();
            if matches.is_empty() {
                return Err(Status::not_found(format!("network '{name}' not found")));
            }
            if matches.len() > 1 {
                return Err(Status::failed_precondition(format!(
                    "network '{name}' exists on multiple nodes; pass target_node"
                )));
            }
            self.db
                .get_node(&matches[0].node_id)
                .map_err(|e| Status::internal(e.to_string()))?
                .ok_or_else(|| {
                    Status::not_found(format!("node '{}' not found", matches[0].node_id))
                })?
        };

        let in_use = self
            .db
            .list_vms_for_node(&node.id)
            .map_err(|e| Status::internal(format!("listing vms: {e}")))?
            .into_iter()
            .any(|vm| vm.network == name);
        if in_use {
            return Err(Status::failed_precondition(format!(
                "network '{name}' is still in use by at least one VM on node '{}'",
                node.id
            )));
        }

        let was_vxlan = self
            .db
            .get_network_for_node(&node.id, name)
            .map_err(|e| Status::internal(format!("reading network: {e}")))?
            .map(|n| n.network_type == "vxlan")
            .unwrap_or(false);

        let deleted = self
            .db
            .delete_network(&node.id, name)
            .map_err(|e| Status::internal(format!("deleting network: {e}")))?;
        if !deleted {
            return Err(Status::not_found(format!(
                "network '{name}' not found on node '{}'",
                node.id
            )));
        }

        self.push_config_to_node(&node).await?;

        if was_vxlan {
            self.refresh_vxlan_peers(name, &node.id).await;
        }

        self.log_replication_event(
            &actor,
            Some("DeleteNetwork"),
            EVT_NETWORK_DELETE,
            &format!("network/{}/{}", node.id, name),
            serde_json::json!({
                "name": name,
                "nodeId": node.id,
            }),
        );
        Ok(Response::new(controller_proto::DeleteNetworkResponse {
            success: true,
        }))
    }

    async fn list_networks(
        &self,
        request: Request<controller_proto::ListNetworksRequest>,
    ) -> Result<Response<controller_proto::ListNetworksResponse>, Status> {
        self.require_operator(&request, OperatorRole::ReadOnly)?;
        let req = request.into_inner();
        let rows = if req.target_node.is_empty() {
            self.db
                .list_networks()
                .map_err(|e| Status::internal(format!("listing networks: {e}")))?
        } else {
            let node = self
                .db
                .get_node_by_address(&req.target_node)
                .map_err(|e| Status::internal(e.to_string()))?
                .or_else(|| self.db.get_node(&req.target_node).ok().flatten())
                .ok_or_else(|| Status::not_found(format!("node {} not found", req.target_node)))?;
            self.db
                .list_networks_for_node(&node.id)
                .map_err(|e| Status::internal(format!("listing networks for node: {e}")))?
        };

        let mut networks = Vec::with_capacity(rows.len());
        for n in rows {
            let policy = self
                .db
                .get_network_policy(&n.node_id, &n.name)
                .map_err(|e| Status::internal(format!("reading network policy: {e}")))?;
            networks.push(controller_proto::NetworkInfo {
                name: n.name,
                external_ip: n.external_ip,
                gateway_ip: n.gateway_ip,
                internal_netmask: n.internal_netmask,
                node_id: n.node_id,
                allowed_tcp_ports: parse_port_list(&n.allowed_tcp_ports),
                allowed_udp_ports: parse_port_list(&n.allowed_udp_ports),
                vlan_id: n.vlan_id,
                network_type: n.network_type,
                enable_outbound_nat: n.enable_outbound_nat,
                ipv6_prefix: policy.ipv6_prefix,
                ipv6_gateway: policy.ipv6_gateway,
                east_west_firewall: policy.east_west,
            });
        }

        Ok(Response::new(controller_proto::ListNetworksResponse {
            networks,
        }))
    }

    async fn create_postgresql(
        &self,
        request: Request<controller_proto::CreatePostgresqlRequest>,
    ) -> Result<Response<controller_proto::CreatePostgresqlResponse>, Status> {
        self.require_operator(&request, OperatorRole::VmAdmin)?;
        let actor = Self::audit_actor(&request);
        let req = request.into_inner();
        let name = validate_postgresql_name(&req.name)?;
        let package = validate_postgresql_package(&req.package)?;
        let port = validate_postgresql_port(req.port)?;
        let database = if req.database.trim().is_empty() {
            validate_postgresql_database(&name).map_err(|_| {
                Status::invalid_argument(
                    "database is required when the instance name is not a PostgreSQL identifier",
                )
            })?
        } else {
            validate_postgresql_database(&req.database)?
        };

        if let Some(existing) = self
            .db
            .get_postgresql(&name)
            .map_err(|e| Status::internal(format!("checking postgresql: {e}")))?
        {
            let node = if req.target_node.trim().is_empty() {
                self.lookup_node(&existing.node_id)?
            } else {
                self.lookup_node(&req.target_node)?
            };
            if node.id != existing.node_id {
                return Err(Status::invalid_argument(format!(
                    "cannot change immutable field target_node on postgresql '{name}' \
                     (delete it and recreate)"
                )));
            }
            if existing.database_name != database {
                return Err(Status::invalid_argument(format!(
                    "cannot change immutable field database on postgresql '{name}' \
                     (delete it and recreate)"
                )));
            }
            if existing.package != package {
                return Err(Status::invalid_argument(format!(
                    "cannot change immutable field package on postgresql '{name}' \
                     (delete it and recreate)"
                )));
            }
            if existing.port == port {
                return Ok(Response::new(controller_proto::CreatePostgresqlResponse {
                    success: true,
                    message: format!("postgresql '{name}' on node '{}' unchanged", node.id),
                    node_id: node.id.clone(),
                    action: controller_proto::ApplyAction::Unchanged as i32,
                    changed_fields: Vec::new(),
                    postgresql: Some(postgresql_info(&existing)),
                }));
            }
            self.db
                .update_postgresql_port(&name, port)
                .map_err(|e| Status::internal(format!("updating postgresql port: {e}")))?;
            let updated = self
                .db
                .get_postgresql(&name)
                .map_err(|e| Status::internal(format!("reading postgresql: {e}")))?
                .ok_or_else(|| Status::internal("postgresql disappeared during update"))?;
            if let Err(err) = self.push_config_to_node(&node).await {
                let _ = self.db.update_postgresql_port(&name, existing.port);
                return Err(err);
            }
            self.log_replication_event(
                &actor,
                Some("CreatePostgresql"),
                EVT_POSTGRESQL_UPDATE,
                &format!("postgresql/{name}"),
                postgresql_replication_body(&updated),
            );
            return Ok(Response::new(controller_proto::CreatePostgresqlResponse {
                success: true,
                message: format!("updated postgresql '{name}' on node '{}'", node.id),
                node_id: node.id,
                action: controller_proto::ApplyAction::Updated as i32,
                changed_fields: vec!["port".to_string()],
                postgresql: Some(postgresql_info(&updated)),
            }));
        }

        let node = if req.target_node.trim().is_empty() {
            let occupied = self
                .db
                .list_postgresqls()
                .map_err(|e| Status::internal(format!("listing postgresql: {e}")))?
                .into_iter()
                .map(|row| row.node_id)
                .collect::<HashSet<_>>();
            let nodes = self
                .db
                .list_nodes()
                .map_err(|e| Status::internal(e.to_string()))?;
            let candidates: Vec<NodeRow> = nodes
                .into_iter()
                .filter(|candidate| !occupied.contains(&candidate.id))
                .collect();
            scheduler::select_node(&candidates)
                .cloned()
                .ok_or_else(|| {
                    Status::failed_precondition(
                        "no ready node is free for PostgreSQL; v1 runs one database per node",
                    )
                })?
        } else {
            self.lookup_node(&req.target_node)?
        };

        if let Some(other) = self
            .db
            .get_postgresql_for_node(&node.id)
            .map_err(|e| Status::internal(format!("checking node postgresql: {e}")))?
        {
            return Err(Status::failed_precondition(format!(
                "node '{}' already runs postgresql '{}'; v1 is a single database per node",
                node.id, other.name
            )));
        }

        let row = PostgresqlRow {
            name: name.clone(),
            database_name: database,
            package,
            port,
            node_id: node.id.clone(),
            created_at: String::new(),
        };
        self.db
            .insert_postgresql(&row)
            .map_err(|e| Status::internal(format!("storing postgresql: {e}")))?;
        if let Err(err) = self.push_config_to_node(&node).await {
            let _ = self.db.delete_postgresql(&name);
            return Err(err);
        }
        self.log_replication_event(
            &actor,
            Some("CreatePostgresql"),
            EVT_POSTGRESQL_CREATE,
            &format!("postgresql/{name}"),
            postgresql_replication_body(&row),
        );
        Ok(Response::new(controller_proto::CreatePostgresqlResponse {
            success: true,
            message: format!(
                "created postgresql '{name}' database '{}' on node '{}' using pkgs.{}",
                row.database_name, node.id, row.package
            ),
            node_id: node.id,
            action: controller_proto::ApplyAction::Created as i32,
            changed_fields: Vec::new(),
            postgresql: Some(postgresql_info(&row)),
        }))
    }

    async fn delete_postgresql(
        &self,
        request: Request<controller_proto::DeletePostgresqlRequest>,
    ) -> Result<Response<controller_proto::DeletePostgresqlResponse>, Status> {
        self.require_operator(&request, OperatorRole::VmAdmin)?;
        let actor = Self::audit_actor(&request);
        let req = request.into_inner();
        let name = validate_postgresql_name(&req.name)?;
        let existing = self
            .db
            .get_postgresql(&name)
            .map_err(|e| Status::internal(format!("reading postgresql: {e}")))?
            .ok_or_else(|| Status::not_found(format!("postgresql '{name}' not found")))?;
        let node = self
            .db
            .get_node(&existing.node_id)
            .map_err(|e| Status::internal(format!("reading node {}: {e}", existing.node_id)))?;
        self.db
            .delete_postgresql(&name)
            .map_err(|e| Status::internal(format!("deleting postgresql: {e}")))?;
        if let Some(node) = node.as_ref() {
            if let Err(err) = self.push_config_to_node(node).await {
                let _ = self.db.insert_postgresql(&existing);
                return Err(err);
            }
        }
        self.log_replication_event(
            &actor,
            Some("DeletePostgresql"),
            EVT_POSTGRESQL_DELETE,
            &format!("postgresql/{name}"),
            postgresql_replication_body(&existing),
        );
        let message = if node.is_some() {
            format!(
                "deleted postgresql '{name}' from node '{}'. The NixOS service is removed from the node config; /var/lib/postgresql is left on disk",
                existing.node_id
            )
        } else {
            format!(
                "deleted postgresql '{name}'. Node '{}' is gone, so no config was pushed",
                existing.node_id
            )
        };
        Ok(Response::new(controller_proto::DeletePostgresqlResponse {
            success: true,
            message,
        }))
    }

    async fn get_postgresql(
        &self,
        request: Request<controller_proto::GetPostgresqlRequest>,
    ) -> Result<Response<controller_proto::GetPostgresqlResponse>, Status> {
        self.require_operator(&request, OperatorRole::ReadOnly)?;
        let req = request.into_inner();
        let name = validate_postgresql_name(&req.name)?;
        let row = self
            .db
            .get_postgresql(&name)
            .map_err(|e| Status::internal(format!("reading postgresql: {e}")))?
            .ok_or_else(|| Status::not_found(format!("postgresql '{name}' not found")))?;
        Ok(Response::new(controller_proto::GetPostgresqlResponse {
            postgresql: Some(postgresql_info(&row)),
        }))
    }

    async fn list_postgresqls(
        &self,
        request: Request<controller_proto::ListPostgresqlsRequest>,
    ) -> Result<Response<controller_proto::ListPostgresqlsResponse>, Status> {
        self.require_operator(&request, OperatorRole::ReadOnly)?;
        let req = request.into_inner();
        let rows = if req.target_node.trim().is_empty() {
            self.db
                .list_postgresqls()
                .map_err(|e| Status::internal(format!("listing postgresql: {e}")))?
        } else {
            let node = self.lookup_node(&req.target_node)?;
            self.db
                .get_postgresql_for_node(&node.id)
                .map_err(|e| Status::internal(format!("listing postgresql: {e}")))?
                .into_iter()
                .collect()
        };
        Ok(Response::new(controller_proto::ListPostgresqlsResponse {
            postgresqls: rows.iter().map(postgresql_info).collect(),
        }))
    }

    async fn create_security_group(
        &self,
        request: Request<controller_proto::CreateSecurityGroupRequest>,
    ) -> Result<Response<controller_proto::CreateSecurityGroupResponse>, Status> {
        self.require_operator(&request, OperatorRole::VmAdmin)?;
        let actor = Self::audit_actor(&request);
        let req = request.into_inner();
        let sg = req
            .security_group
            .ok_or_else(|| Status::invalid_argument("security_group is required"))?;
        let name = validate_network_name(&sg.name)?;

        // Build the normalized incoming rule set (with ids + validations). We
        // need these whether this is a create or an upsert so diffing can
        // compare apples to apples.
        let mut rules = Vec::new();
        for r in &sg.rules {
            let id = if r.id.trim().is_empty() {
                Uuid::new_v4().to_string()
            } else {
                r.id.trim().to_string()
            };
            let protocol = normalize_sg_protocol(&r.protocol)?;
            let host_port = validate_port(r.host_port, "host_port")?;
            let target_port = if r.target_port <= 0 {
                host_port
            } else {
                validate_port(r.target_port, "target_port")?
            };
            rules.push(SecurityGroupRuleRow {
                id,
                security_group: name.clone(),
                protocol,
                host_port,
                target_port,
                source_cidr: r.source_cidr.trim().to_string(),
                target_vm: r.target_vm.trim().to_string(),
                enable_dnat: r.enable_dnat,
            });
        }
        let normalized_proto_rules: Vec<controller_proto::SecurityGroupRule> = rules
            .iter()
            .map(|r| controller_proto::SecurityGroupRule {
                id: r.id.clone(),
                protocol: r.protocol.clone(),
                host_port: r.host_port,
                target_port: r.target_port,
                source_cidr: r.source_cidr.clone(),
                target_vm: r.target_vm.clone(),
                enable_dnat: r.enable_dnat,
            })
            .collect();

        // Upsert: if a security group with this name already exists, run a
        // diff and either no-op (UNCHANGED) or replace mutable fields
        // (description/rules) and return UPDATED.
        let existing_sg = self
            .db
            .get_security_group(&name)
            .map_err(|e| Status::internal(e.to_string()))?;
        let incoming_description = sg.description.trim().to_string();

        let (action, changed_fields) = if let Some(existing) = existing_sg {
            let stored_rules = self
                .db
                .list_security_group_rules(&name)
                .map_err(|e| Status::internal(e.to_string()))?;
            let apply = crate::grpc::diff::SecurityGroupApply {
                description: &incoming_description,
                rules: &normalized_proto_rules,
            };
            let d = crate::grpc::diff::diff_security_group(&existing, &stored_rules, &apply);
            if !d.immutable.is_empty() {
                return Err(Status::invalid_argument(format!(
                    "cannot change immutable field(s) on security group '{name}': {} \
                     (delete the security group and recreate)",
                    d.immutable.join(", ")
                )));
            }
            if d.mutable.is_empty() {
                let rules_resp: Vec<controller_proto::SecurityGroupRule> = stored_rules
                    .into_iter()
                    .map(|r| controller_proto::SecurityGroupRule {
                        id: r.id,
                        protocol: r.protocol,
                        host_port: r.host_port,
                        target_port: r.target_port,
                        source_cidr: r.source_cidr,
                        target_vm: r.target_vm,
                        enable_dnat: r.enable_dnat,
                    })
                    .collect();
                return Ok(Response::new(
                    controller_proto::CreateSecurityGroupResponse {
                        success: true,
                        security_group: Some(controller_proto::SecurityGroup {
                            name: existing.name,
                            description: existing.description,
                            rules: rules_resp,
                            created_at: parse_datetime_to_timestamp(&existing.created_at),
                        }),
                        action: controller_proto::ApplyAction::Unchanged as i32,
                        changed_fields: Vec::new(),
                    },
                ));
            }
            (
                controller_proto::ApplyAction::Updated as i32,
                d.mutable.clone(),
            )
        } else {
            (controller_proto::ApplyAction::Created as i32, Vec::new())
        };

        let row = SecurityGroupRow {
            name: name.clone(),
            description: incoming_description.clone(),
            created_at: String::new(),
        };
        self.db
            .upsert_security_group(&row)
            .map_err(|e| Status::internal(format!("upserting security group: {e}")))?;

        self.db
            .replace_security_group_rules(&name, &rules)
            .map_err(|e| Status::internal(format!("storing security group rules: {e}")))?;

        self.log_replication_event(
            &actor,
            Some("CreateSecurityGroup"),
            EVT_SECURITY_GROUP_CREATE,
            &format!("security-group/{name}"),
            serde_json::json!({
                "name": name,
                "description": row.description,
                "rules": rules.iter().map(|r| serde_json::json!({
                    "id": r.id,
                    "protocol": r.protocol,
                    "hostPort": r.host_port,
                    "targetPort": r.target_port,
                    "sourceCidr": r.source_cidr,
                    "targetVm": r.target_vm,
                    "enableDnat": r.enable_dnat,
                })).collect::<Vec<_>>(),
                "action": match action {
                    x if x == controller_proto::ApplyAction::Created as i32 => "created",
                    x if x == controller_proto::ApplyAction::Updated as i32 => "updated",
                    _ => "unchanged",
                },
                "changedFields": changed_fields,
            }),
        );

        let created = self
            .db
            .get_security_group(&name)
            .map_err(|e| Status::internal(e.to_string()))?
            .ok_or_else(|| Status::internal("security group disappeared after create"))?;
        let stored_rules = self
            .db
            .list_security_group_rules(&name)
            .map_err(|e| Status::internal(e.to_string()))?;

        Ok(Response::new(
            controller_proto::CreateSecurityGroupResponse {
                success: true,
                security_group: Some(controller_proto::SecurityGroup {
                    name: created.name,
                    description: created.description,
                    rules: stored_rules
                        .into_iter()
                        .map(|r| controller_proto::SecurityGroupRule {
                            id: r.id,
                            protocol: r.protocol,
                            host_port: r.host_port,
                            target_port: r.target_port,
                            source_cidr: r.source_cidr,
                            target_vm: r.target_vm,
                            enable_dnat: r.enable_dnat,
                        })
                        .collect(),
                    created_at: parse_datetime_to_timestamp(&created.created_at),
                }),
                action,
                changed_fields,
            },
        ))
    }

    async fn get_security_group(
        &self,
        request: Request<controller_proto::GetSecurityGroupRequest>,
    ) -> Result<Response<controller_proto::GetSecurityGroupResponse>, Status> {
        self.require_operator(&request, OperatorRole::ReadOnly)?;
        let req = request.into_inner();
        let name = req.name.trim();
        if name.is_empty() {
            return Err(Status::invalid_argument("name is required"));
        }
        let sg = self
            .db
            .get_security_group(name)
            .map_err(|e| Status::internal(e.to_string()))?
            .ok_or_else(|| Status::not_found(format!("security group '{name}' not found")))?;
        let rules = self
            .db
            .list_security_group_rules(name)
            .map_err(|e| Status::internal(e.to_string()))?;
        let vm_atts = self
            .db
            .list_security_group_vm_attachments(name)
            .map_err(|e| Status::internal(e.to_string()))?;
        let net_atts = self
            .db
            .list_security_group_network_attachments(name)
            .map_err(|e| Status::internal(e.to_string()))?;
        let mut attachments = Vec::new();
        attachments.extend(vm_atts.into_iter().map(|a| {
            controller_proto::SecurityGroupAttachment {
                security_group: a.security_group,
                target_kind: controller_proto::SecurityGroupTargetKind::Vm as i32,
                target_id: a.vm_id,
                target_node: String::new(),
            }
        }));
        attachments.extend(net_atts.into_iter().map(|a| {
            controller_proto::SecurityGroupAttachment {
                security_group: a.security_group,
                target_kind: controller_proto::SecurityGroupTargetKind::Network as i32,
                target_id: a.network_name,
                target_node: a.node_id,
            }
        }));

        Ok(Response::new(controller_proto::GetSecurityGroupResponse {
            security_group: Some(controller_proto::SecurityGroup {
                name: sg.name,
                description: sg.description,
                rules: rules
                    .into_iter()
                    .map(|r| controller_proto::SecurityGroupRule {
                        id: r.id,
                        protocol: r.protocol,
                        host_port: r.host_port,
                        target_port: r.target_port,
                        source_cidr: r.source_cidr,
                        target_vm: r.target_vm,
                        enable_dnat: r.enable_dnat,
                    })
                    .collect(),
                created_at: parse_datetime_to_timestamp(&sg.created_at),
            }),
            attachments,
        }))
    }

    async fn list_security_groups(
        &self,
        request: Request<controller_proto::ListSecurityGroupsRequest>,
    ) -> Result<Response<controller_proto::ListSecurityGroupsResponse>, Status> {
        self.require_operator(&request, OperatorRole::ReadOnly)?;
        let groups = self
            .db
            .list_security_groups()
            .map_err(|e| Status::internal(e.to_string()))?;
        let mut out = Vec::new();
        for sg in groups {
            let rules = self
                .db
                .list_security_group_rules(&sg.name)
                .map_err(|e| Status::internal(e.to_string()))?;
            out.push(controller_proto::SecurityGroup {
                name: sg.name,
                description: sg.description,
                rules: rules
                    .into_iter()
                    .map(|r| controller_proto::SecurityGroupRule {
                        id: r.id,
                        protocol: r.protocol,
                        host_port: r.host_port,
                        target_port: r.target_port,
                        source_cidr: r.source_cidr,
                        target_vm: r.target_vm,
                        enable_dnat: r.enable_dnat,
                    })
                    .collect(),
                created_at: parse_datetime_to_timestamp(&sg.created_at),
            });
        }
        Ok(Response::new(
            controller_proto::ListSecurityGroupsResponse {
                security_groups: out,
            },
        ))
    }

    async fn delete_security_group(
        &self,
        request: Request<controller_proto::DeleteSecurityGroupRequest>,
    ) -> Result<Response<controller_proto::DeleteSecurityGroupResponse>, Status> {
        self.require_operator(&request, OperatorRole::VmAdmin)?;
        let actor = Self::audit_actor(&request);
        let req = request.into_inner();
        let name = req.name.trim();
        if name.is_empty() {
            return Err(Status::invalid_argument("name is required"));
        }
        let deleted = self
            .db
            .delete_security_group(name)
            .map_err(|e| Status::internal(e.to_string()))?;
        if !deleted {
            return Err(Status::not_found(format!(
                "security group '{name}' not found"
            )));
        }
        self.log_replication_event(
            &actor,
            Some("DeleteSecurityGroup"),
            EVT_SECURITY_GROUP_DELETE,
            &format!("security-group/{name}"),
            serde_json::json!({ "name": name }),
        );
        Ok(Response::new(
            controller_proto::DeleteSecurityGroupResponse { success: true },
        ))
    }

    async fn attach_security_group(
        &self,
        request: Request<controller_proto::AttachSecurityGroupRequest>,
    ) -> Result<Response<controller_proto::AttachSecurityGroupResponse>, Status> {
        self.require_operator(&request, OperatorRole::VmAdmin)?;
        let actor = Self::audit_actor(&request);
        let req = request.into_inner();
        let sg = req.security_group.trim();
        if sg.is_empty() {
            return Err(Status::invalid_argument("security_group is required"));
        }
        if self
            .db
            .get_security_group(sg)
            .map_err(|e| Status::internal(e.to_string()))?
            .is_none()
        {
            return Err(Status::not_found(format!(
                "security group '{sg}' not found"
            )));
        }
        let kind = parse_sg_target_kind(req.target_kind)?;
        match kind {
            controller_proto::SecurityGroupTargetKind::Vm => {
                let vm = self
                    .db
                    .get_vm(&req.target_id)
                    .map_err(|e| Status::internal(e.to_string()))?
                    .or_else(|| {
                        self.db
                            .list_vms()
                            .ok()
                            .and_then(|vms| vms.into_iter().find(|v| v.name == req.target_id))
                    })
                    .ok_or_else(|| {
                        Status::not_found(format!("vm '{}' not found", req.target_id))
                    })?;
                self.db
                    .attach_security_group_to_vm(sg, &vm.id)
                    .map_err(|e| Status::internal(e.to_string()))?;
                if let Some(node) = self
                    .db
                    .get_node(&vm.node_id)
                    .map_err(|e| Status::internal(e.to_string()))?
                {
                    self.push_config_to_node(&node).await?;
                }
            }
            controller_proto::SecurityGroupTargetKind::Network => {
                let node = if !req.target_node.trim().is_empty() {
                    self.db
                        .get_node_by_address(req.target_node.trim())
                        .map_err(|e| Status::internal(e.to_string()))?
                        .or_else(|| self.db.get_node(req.target_node.trim()).ok().flatten())
                        .ok_or_else(|| {
                            Status::not_found(format!("node '{}' not found", req.target_node))
                        })?
                } else {
                    return Err(Status::invalid_argument(
                        "target_node is required for network attachments",
                    ));
                };
                if self
                    .db
                    .get_network_for_node(&node.id, req.target_id.trim())
                    .map_err(|e| Status::internal(e.to_string()))?
                    .is_none()
                {
                    return Err(Status::not_found(format!(
                        "network '{}' not found on node '{}'",
                        req.target_id.trim(),
                        node.id
                    )));
                }
                self.db
                    .attach_security_group_to_network(sg, req.target_id.trim(), &node.id)
                    .map_err(|e| Status::internal(e.to_string()))?;
                self.push_config_to_node(&node).await?;
            }
            controller_proto::SecurityGroupTargetKind::Unspecified => unreachable!(),
        }
        self.log_replication_event(
            &actor,
            Some("AttachSecurityGroup"),
            EVT_SECURITY_GROUP_ATTACH,
            &format!("security-group/{sg}"),
            serde_json::json!({
                "securityGroup": sg,
                "targetKind": req.target_kind,
                "targetId": req.target_id,
                "targetNode": req.target_node
            }),
        );
        Ok(Response::new(
            controller_proto::AttachSecurityGroupResponse { success: true },
        ))
    }

    async fn detach_security_group(
        &self,
        request: Request<controller_proto::DetachSecurityGroupRequest>,
    ) -> Result<Response<controller_proto::DetachSecurityGroupResponse>, Status> {
        self.require_operator(&request, OperatorRole::VmAdmin)?;
        let actor = Self::audit_actor(&request);
        let req = request.into_inner();
        let sg = req.security_group.trim();
        if sg.is_empty() {
            return Err(Status::invalid_argument("security_group is required"));
        }
        let kind = parse_sg_target_kind(req.target_kind)?;
        match kind {
            controller_proto::SecurityGroupTargetKind::Vm => {
                let vm = self
                    .db
                    .get_vm(&req.target_id)
                    .map_err(|e| Status::internal(e.to_string()))?
                    .or_else(|| {
                        self.db
                            .list_vms()
                            .ok()
                            .and_then(|vms| vms.into_iter().find(|v| v.name == req.target_id))
                    })
                    .ok_or_else(|| {
                        Status::not_found(format!("vm '{}' not found", req.target_id))
                    })?;
                self.db
                    .detach_security_group_from_vm(sg, &vm.id)
                    .map_err(|e| Status::internal(e.to_string()))?;
                if let Some(node) = self
                    .db
                    .get_node(&vm.node_id)
                    .map_err(|e| Status::internal(e.to_string()))?
                {
                    self.push_config_to_node(&node).await?;
                }
            }
            controller_proto::SecurityGroupTargetKind::Network => {
                let node = if !req.target_node.trim().is_empty() {
                    self.db
                        .get_node_by_address(req.target_node.trim())
                        .map_err(|e| Status::internal(e.to_string()))?
                        .or_else(|| self.db.get_node(req.target_node.trim()).ok().flatten())
                        .ok_or_else(|| {
                            Status::not_found(format!("node '{}' not found", req.target_node))
                        })?
                } else {
                    return Err(Status::invalid_argument(
                        "target_node is required for network attachments",
                    ));
                };
                self.db
                    .detach_security_group_from_network(sg, req.target_id.trim(), &node.id)
                    .map_err(|e| Status::internal(e.to_string()))?;
                self.push_config_to_node(&node).await?;
            }
            controller_proto::SecurityGroupTargetKind::Unspecified => unreachable!(),
        }
        self.log_replication_event(
            &actor,
            Some("DetachSecurityGroup"),
            EVT_SECURITY_GROUP_DETACH,
            &format!("security-group/{sg}"),
            serde_json::json!({
                "securityGroup": sg,
                "targetKind": req.target_kind,
                "targetId": req.target_id,
                "targetNode": req.target_node
            }),
        );
        Ok(Response::new(
            controller_proto::DetachSecurityGroupResponse { success: true },
        ))
    }

    async fn list_gpus(
        &self,
        request: Request<controller_proto::ListGpusRequest>,
    ) -> Result<Response<controller_proto::ListGpusResponse>, Status> {
        self.require_operator(&request, OperatorRole::ReadOnly)?;
        let req = request.into_inner();
        let filter = if req.node_id.trim().is_empty() {
            None
        } else {
            Some(req.node_id.trim())
        };
        let rows = self
            .db
            .list_node_gpus(filter)
            .map_err(|e| Status::internal(format!("listing GPUs: {e}")))?;
        let vms = self
            .db
            .list_vms()
            .map_err(|e| Status::internal(format!("listing VMs: {e}")))?;
        let mut assigned: std::collections::HashMap<(String, String), String> =
            std::collections::HashMap::new();
        for vm in &vms {
            for name in self.vm_gpu_names(&vm.id)? {
                assigned.insert(
                    (
                        vm.node_id.clone(),
                        kcore_gpu::binding_device_name(&name).to_string(),
                    ),
                    vm.name.clone(),
                );
            }
        }
        let gpus = rows
            .into_iter()
            .filter(|row| row.kind.is_empty() || row.kind == "gpu" || row.kind == "vgpu")
            .map(|row| gpu_info_from_row(row, &assigned))
            .collect();
        Ok(Response::new(controller_proto::ListGpusResponse { gpus }))
    }

    async fn list_pci_devices(
        &self,
        request: Request<controller_proto::ListGpusRequest>,
    ) -> Result<Response<controller_proto::ListGpusResponse>, Status> {
        self.require_operator(&request, OperatorRole::ReadOnly)?;
        let req = request.into_inner();
        let filter = if req.node_id.trim().is_empty() {
            None
        } else {
            Some(req.node_id.trim())
        };
        let rows = self
            .db
            .list_node_gpus(filter)
            .map_err(|e| Status::internal(format!("listing PCI devices: {e}")))?;
        let vms = self
            .db
            .list_vms()
            .map_err(|e| Status::internal(format!("listing VMs: {e}")))?;
        let mut assigned: std::collections::HashMap<(String, String), String> =
            std::collections::HashMap::new();
        for vm in &vms {
            for name in self.vm_gpu_names(&vm.id)? {
                assigned.insert(
                    (
                        vm.node_id.clone(),
                        kcore_gpu::binding_device_name(&name).to_string(),
                    ),
                    vm.name.clone(),
                );
            }
            let owned = crate::pci::split_pci_devices(&vm.pci_devices);
            for row in &rows {
                if row.node_id == vm.node_id
                    && crate::pci::split_pci_devices(&row.pci_devices)
                        .iter()
                        .any(|addr| owned.iter().any(|have| have == addr))
                {
                    assigned
                        .entry((row.node_id.clone(), row.name.clone()))
                        .or_insert_with(|| vm.name.clone());
                }
            }
        }
        let gpus = rows
            .into_iter()
            .map(|row| gpu_info_from_row(row, &assigned))
            .collect();
        Ok(Response::new(controller_proto::ListGpusResponse { gpus }))
    }

    async fn list_nodes(
        &self,
        request: Request<controller_proto::ListNodesRequest>,
    ) -> Result<Response<controller_proto::ListNodesResponse>, Status> {
        self.require_operator(&request, OperatorRole::ReadOnly)?;
        let nodes = self
            .db
            .list_nodes()
            .map_err(|e| Status::internal(e.to_string()))?;

        let all_labels = self.db.get_all_node_labels().unwrap_or_default();

        let infos = nodes
            .into_iter()
            .map(|n| {
                let labels: Vec<String> = all_labels
                    .iter()
                    .filter(|(nid, _)| nid == &n.id)
                    .map(|(_, l)| l.clone())
                    .collect();
                let hb = if n.last_heartbeat.is_empty() {
                    None
                } else {
                    parse_datetime_to_timestamp(&n.last_heartbeat)
                };
                let (
                    kcore_version,
                    booted_generation,
                    nixos_generation,
                    update_capabilities,
                    schema_version,
                    nixpkgs_rev,
                ) = host_status_fields(&self.db, &n.id);
                controller_proto::NodeInfo {
                    node_id: n.id,
                    hostname: n.hostname,
                    address: n.address,
                    capacity: Some(controller_proto::NodeCapacity {
                        cpu_cores: n.cpu_cores,
                        memory_bytes: n.memory_bytes,
                    }),
                    usage: Some(controller_proto::NodeUsage {
                        cpu_cores_used: n.cpu_used,
                        memory_bytes_used: n.memory_used,
                    }),
                    status: n.status,
                    last_heartbeat: hb,
                    labels,
                    storage_backend: storage_backend_to_proto(&n.storage_backend),
                    disable_vxlan: n.disable_vxlan,
                    approval_status: n.approval_status,
                    cert_expiry_days: n.cert_expiry_days,
                    luks_method: n.luks_method,
                    dc_id: n.dc_id,
                    kcore_version,
                    booted_generation,
                    nixos_generation,
                    update_capabilities,
                    schema_version,
                    nixpkgs_rev,
                }
            })
            .collect();

        Ok(Response::new(controller_proto::ListNodesResponse {
            nodes: infos,
        }))
    }

    async fn get_node(
        &self,
        request: Request<controller_proto::GetNodeRequest>,
    ) -> Result<Response<controller_proto::GetNodeResponse>, Status> {
        self.require_operator(&request, OperatorRole::ReadOnly)?;
        let req = request.into_inner();
        let node = self
            .db
            .get_node(&req.node_id)
            .map_err(|e| Status::internal(e.to_string()))?
            .ok_or_else(|| Status::not_found(format!("node {} not found", req.node_id)))?;

        let labels = self.db.get_node_labels(&node.id).unwrap_or_default();
        let hb = if node.last_heartbeat.is_empty() {
            None
        } else {
            parse_datetime_to_timestamp(&node.last_heartbeat)
        };

        let (
            kcore_version,
            booted_generation,
            nixos_generation,
            update_capabilities,
            schema_version,
            nixpkgs_rev,
        ) = host_status_fields(&self.db, &node.id);
        Ok(Response::new(controller_proto::GetNodeResponse {
            node: Some(controller_proto::NodeInfo {
                node_id: node.id.clone(),
                hostname: node.hostname,
                address: node.address,
                capacity: Some(controller_proto::NodeCapacity {
                    cpu_cores: node.cpu_cores,
                    memory_bytes: node.memory_bytes,
                }),
                usage: Some(controller_proto::NodeUsage {
                    cpu_cores_used: node.cpu_used,
                    memory_bytes_used: node.memory_used,
                }),
                status: node.status,
                last_heartbeat: hb,
                labels,
                storage_backend: storage_backend_to_proto(&node.storage_backend),
                disable_vxlan: node.disable_vxlan,
                approval_status: node.approval_status,
                cert_expiry_days: node.cert_expiry_days,
                luks_method: node.luks_method,
                dc_id: node.dc_id,
                kcore_version,
                booted_generation,
                nixos_generation,
                update_capabilities,
                schema_version,
                nixpkgs_rev,
            }),
        }))
    }

    async fn create_ssh_key(
        &self,
        request: Request<controller_proto::CreateSshKeyRequest>,
    ) -> Result<Response<controller_proto::CreateSshKeyResponse>, Status> {
        self.require_operator(&request, OperatorRole::VmAdmin)?;
        let actor = Self::audit_actor(&request);
        let req = request.into_inner();

        if req.name.trim().is_empty() {
            return Err(Status::invalid_argument("name is required"));
        }
        if req.public_key.trim().is_empty() {
            return Err(Status::invalid_argument("public_key is required"));
        }
        if !req.public_key.starts_with("ssh-") && !req.public_key.starts_with("ecdsa-") {
            return Err(Status::invalid_argument(
                "public_key must start with ssh- or ecdsa- (OpenSSH format)",
            ));
        }

        let name = req.name.trim();
        let public_key = req.public_key.trim();

        // Upsert: if an SSH key with this name exists, compare public keys.
        // public_key is immutable in v1 — any change is rejected so the user
        // explicitly deletes and recreates (which invalidates old workloads).
        if let Some((_, stored_pk, _)) = self
            .db
            .get_ssh_key(name)
            .map_err(|e| Status::internal(format!("fetching ssh key: {e}")))?
        {
            let diff = crate::grpc::diff::diff_ssh_key(&stored_pk, public_key);
            if !diff.immutable.is_empty() {
                return Err(Status::invalid_argument(format!(
                    "cannot change immutable field(s) on SSH key '{name}': {} \
                     (delete the key and recreate)",
                    diff.immutable.join(", ")
                )));
            }
            return Ok(Response::new(controller_proto::CreateSshKeyResponse {
                success: true,
                message: format!("SSH key '{name}' unchanged"),
                action: controller_proto::ApplyAction::Unchanged as i32,
                changed_fields: Vec::new(),
            }));
        }

        self.db
            .insert_ssh_key(name, public_key)
            .map_err(|e| Status::internal(format!("storing ssh key: {e}")))?;

        info!(name = %req.name, "created SSH key");
        self.log_replication_event(
            &actor,
            Some("CreateSshKey"),
            EVT_SSH_KEY_CREATE,
            &format!("ssh-key/{name}"),
            serde_json::json!({
                "name": name,
                "publicKey": public_key,
            }),
        );

        Ok(Response::new(controller_proto::CreateSshKeyResponse {
            success: true,
            message: format!("SSH key '{}' created", req.name),
            action: controller_proto::ApplyAction::Created as i32,
            changed_fields: Vec::new(),
        }))
    }

    async fn delete_ssh_key(
        &self,
        request: Request<controller_proto::DeleteSshKeyRequest>,
    ) -> Result<Response<controller_proto::DeleteSshKeyResponse>, Status> {
        self.require_operator(&request, OperatorRole::VmAdmin)?;
        let actor = Self::audit_actor(&request);
        let req = request.into_inner();

        let deleted = self
            .db
            .delete_ssh_key(&req.name)
            .map_err(|e| Status::internal(format!("deleting ssh key: {e}")))?;

        if !deleted {
            return Err(Status::not_found(format!(
                "SSH key '{}' not found",
                req.name
            )));
        }

        info!(name = %req.name, "deleted SSH key");
        self.log_replication_event(
            &actor,
            Some("DeleteSshKey"),
            EVT_SSH_KEY_DELETE,
            &format!("ssh-key/{}", req.name.trim()),
            serde_json::json!({
                "name": req.name.trim(),
            }),
        );

        Ok(Response::new(controller_proto::DeleteSshKeyResponse {
            success: true,
        }))
    }

    async fn list_ssh_keys(
        &self,
        request: Request<controller_proto::ListSshKeysRequest>,
    ) -> Result<Response<controller_proto::ListSshKeysResponse>, Status> {
        self.require_operator(&request, OperatorRole::ReadOnly)?;

        let keys = self
            .db
            .list_ssh_keys()
            .map_err(|e| Status::internal(format!("listing ssh keys: {e}")))?;

        let infos = keys
            .into_iter()
            .map(|(name, public_key, created_at)| {
                let ts = parse_datetime_to_timestamp(&created_at);
                controller_proto::SshKeyInfo {
                    name,
                    public_key,
                    created_at: ts,
                }
            })
            .collect();

        Ok(Response::new(controller_proto::ListSshKeysResponse {
            keys: infos,
        }))
    }

    async fn get_ssh_key(
        &self,
        request: Request<controller_proto::GetSshKeyRequest>,
    ) -> Result<Response<controller_proto::GetSshKeyResponse>, Status> {
        self.require_operator(&request, OperatorRole::ReadOnly)?;
        let req = request.into_inner();

        let (name, public_key, created_at) = self
            .db
            .get_ssh_key(&req.name)
            .map_err(|e| Status::internal(format!("getting ssh key: {e}")))?
            .ok_or_else(|| Status::not_found(format!("SSH key '{}' not found", req.name)))?;

        let ts = parse_datetime_to_timestamp(&created_at);

        Ok(Response::new(controller_proto::GetSshKeyResponse {
            key: Some(controller_proto::SshKeyInfo {
                name,
                public_key,
                created_at: ts,
            }),
        }))
    }

    async fn drain_node(
        &self,
        request: Request<controller_proto::DrainNodeRequest>,
    ) -> Result<Response<controller_proto::DrainNodeResponse>, Status> {
        self.require_operator(&request, OperatorRole::ClusterAdmin)?;
        let actor = Self::audit_actor(&request);
        let req = request.into_inner();

        let outcome = self
            .drain_node_vms(&req.node_id, &req.target_node, NodeDrainOptions::default())
            .await?;

        let msg = if outcome.errors.is_empty() {
            if outcome.migrated == 0 {
                "node has no VMs, marked as drained".into()
            } else {
                format!("{} VMs migrated successfully", outcome.migrated)
            }
        } else {
            format!(
                "{} VMs migrated, {} errors: {}",
                outcome.migrated,
                outcome.errors.len(),
                outcome.errors.join("; ")
            )
        };
        self.log_replication_event_required(
            &actor,
            Some("DrainNode"),
            EVT_NODE_DRAIN,
            &format!("node/{}", req.node_id),
            serde_json::json!({
                "nodeId": req.node_id,
                "targetNode": req.target_node,
                "migrated": outcome.migrated,
                "errors": outcome.errors,
            }),
        )?;

        Ok(Response::new(controller_proto::DrainNodeResponse {
            success: outcome.errors.is_empty(),
            vms_migrated: outcome.migrated,
            message: msg,
        }))
    }

    async fn migrate_vm(
        &self,
        request: Request<controller_proto::MigrateVmRequest>,
    ) -> Result<Response<controller_proto::MigrateVmResponse>, Status> {
        self.require_operator(&request, OperatorRole::VmAdmin)?;
        let actor = Self::audit_actor(&request);
        let req = request.into_inner();
        if req.vm_id.trim().is_empty() || req.target_node.trim().is_empty() {
            return Err(Status::invalid_argument(
                "vm_id and target_node are required",
            ));
        }

        let vm = self
            .db
            .get_vm(&req.vm_id)
            .map_err(|e| Status::internal(e.to_string()))?
            .or_else(|| {
                self.db
                    .list_vms()
                    .ok()
                    .and_then(|rows| rows.into_iter().find(|v| v.name == req.vm_id))
            })
            .ok_or_else(|| Status::not_found(format!("VM '{}' not found", req.vm_id)))?;

        if !vm.pci_devices.is_empty() || !self.vm_gpu_names(&vm.id)?.is_empty() {
            return self
                .migrate_gpu_vm(
                    &actor,
                    vm,
                    &req.target_node,
                    &req.gpus,
                    &req.vgpus,
                    req.ignore_admission_control,
                )
                .await;
        }

        let source_node = self
            .db
            .get_node(&vm.node_id)
            .map_err(|e| Status::internal(e.to_string()))?
            .ok_or_else(|| Status::not_found(format!("source node '{}' not found", vm.node_id)))?;

        let all_nodes = self
            .db
            .list_nodes()
            .map_err(|e| Status::internal(e.to_string()))?;
        let target = all_nodes
            .iter()
            .find(|n| n.id == req.target_node || n.address == req.target_node)
            .ok_or_else(|| {
                Status::not_found(format!("target node '{}' not found", req.target_node))
            })?;
        if target.id == source_node.id {
            return Err(Status::invalid_argument(
                "target_node must differ from the VM's current node",
            ));
        }
        accepts_migrated_vms(target)?;
        self.admit_move(&actor, &vm, &target.id, req.ignore_admission_control)?;

        if crate::local_migrate::is_local_block(&vm.storage_backend) {
            return self
                .respond_local_migrate(&actor, &vm, &source_node, target, req.allow_cold_fallback)
                .await;
        }

        if vm.storage_backend != "ceph" {
            return Err(Status::failed_precondition(
                "live migrate supports ceph (shared RBD) and lvm/zfs (disk mirror); filesystem disks stay on the node",
            ));
        }
        if !self.node_supports_backend(target, "ceph") {
            return Err(Status::failed_precondition(
                "target node is not a CephCluster member",
            ));
        }
        // Both ends must see the shared pool: an unhealthy cluster means the
        // destination may not be able to map the RBD image we are handing it.
        if !self.is_healthy_ceph_member(&target.id)? {
            return Err(Status::failed_precondition(format!(
                "target node '{}' is not part of a healthy CephCluster",
                target.id
            )));
        }

        let volume = self
            .db
            .get_volume_by_vm(&vm.id)
            .map_err(|e| Status::internal(e.to_string()))?
            .ok_or_else(|| {
                Status::failed_precondition(format!("no Ceph volume row for VM '{}'", vm.name))
            })?;

        let runtime_name = sanitize_nix_attr_key(&vm.name);
        let dest_host = grpc_address_host(&target.address);

        match self
            .live_migrate_vm(
                &vm,
                &source_node,
                target,
                Some(&volume),
                &runtime_name,
                &dest_host,
            )
            .await
        {
            Ok(LiveMigrateOutcome {
                operation_id,
                warnings,
            }) => {
                self.webhooks.emit(
                    crate::webhooks::EVENT_VM_MIGRATED,
                    format!("vm/{}", vm.id),
                    serde_json::json!({
                        "vmId": vm.id,
                        "vmName": vm.name,
                        "sourceNode": source_node.id,
                        "targetNode": target.id,
                        "mode": "live",
                    }),
                );
                self.log_replication_event_required(
                    &actor,
                    Some("MigrateVm"),
                    EVT_VM_MIGRATE,
                    &format!("vm/{}", vm.id),
                    serde_json::json!({
                        "vmId": vm.id,
                        "vmName": vm.name,
                        "sourceNode": source_node.id,
                        "targetNode": target.id,
                        "mode": "live",
                        "operationId": operation_id,
                        "hostWarnings": warnings,
                    }),
                )?;
                let mut message = format!(
                    "live-migrated '{}' from {} to {}",
                    vm.name, source_node.id, target.id
                );
                for line in &warnings {
                    message.push_str("\nwarning: ");
                    message.push_str(line);
                }
                return Ok(Response::new(controller_proto::MigrateVmResponse {
                    success: true,
                    message,
                    mode: "live".into(),
                    source_node: source_node.id,
                    target_node: target.id.clone(),
                    operation_id,
                }));
            }
            Err(live_err) => {
                warn!(
                    vm = %vm.name,
                    send_succeeded = live_err.send_succeeded,
                    operation_id = %live_err.operation_id,
                    error = %live_err.status,
                    "live migrate failed"
                );
                // Never cold-start another CH after send succeeded — that dual-writes RBD.
                if live_err.send_succeeded || !req.allow_cold_fallback {
                    return Err(live_err.status);
                }
                // Surface both failures: an operator debugging a refused
                // fallback needs to know what the live attempt hit first.
                self.cold_reassign_vm(&vm, &source_node, target)
                    .await
                    .map_err(|cold_err| {
                        status_with_context(
                            &cold_err,
                            &format!(
                                "cold fallback for VM '{}' after live migrate failed ({})",
                                vm.name, live_err.status
                            ),
                        )
                    })?;
                self.webhooks.emit(
                    crate::webhooks::EVENT_VM_MIGRATED,
                    format!("vm/{}", vm.id),
                    serde_json::json!({
                        "vmId": vm.id,
                        "vmName": vm.name,
                        "sourceNode": source_node.id,
                        "targetNode": target.id,
                        "mode": "cold",
                    }),
                );
                self.log_replication_event_required(
                    &actor,
                    Some("MigrateVm"),
                    EVT_VM_MIGRATE,
                    &format!("vm/{}", vm.id),
                    serde_json::json!({
                        "vmId": vm.id,
                        "vmName": vm.name,
                        "sourceNode": source_node.id,
                        "targetNode": target.id,
                        "mode": "cold",
                        "liveError": live_err.status.to_string(),
                        "operationId": live_err.operation_id,
                    }),
                )?;
                Ok(Response::new(controller_proto::MigrateVmResponse {
                    success: true,
                    message: format!(
                        "cold-migrated '{}' from {} to {} after live failure: {}",
                        vm.name, source_node.id, target.id, live_err.status
                    ),
                    mode: "cold".into(),
                    source_node: source_node.id,
                    target_node: target.id.clone(),
                    operation_id: live_err.operation_id,
                }))
            }
        }
    }

    async fn get_live_migrate_receive_status(
        &self,
        request: Request<controller_proto::GetLiveMigrateReceiveStatusRequest>,
    ) -> Result<Response<controller_proto::GetLiveMigrateReceiveStatusResponse>, Status> {
        self.require_operator(&request, OperatorRole::ReadOnly)?;
        let req = request.into_inner();
        let vm = self.resolve_vm(&req.vm_id)?;
        let runtime_name = sanitize_nix_attr_key(&vm.name);
        let targets = self.receive_status_targets(&req.node_id)?;

        let mut nodes = Vec::with_capacity(targets.len());
        for node in &targets {
            nodes.push(self.receive_status_for_node(node, &vm, &runtime_name).await);
        }
        let with_session = nodes
            .iter()
            .filter(|n| n.state.as_ref().is_some_and(|s| s.has_session))
            .count();
        Ok(Response::new(
            controller_proto::GetLiveMigrateReceiveStatusResponse {
                success: true,
                message: format!(
                    "queried {} node(s); {} report a prepared receive session for '{}'",
                    nodes.len(),
                    with_session,
                    vm.name
                ),
                vm_id: vm.id,
                vm_name: vm.name,
                runtime_name,
                current_node: vm.node_id,
                nodes,
            },
        ))
    }

    async fn reset_live_migrate_receive(
        &self,
        request: Request<controller_proto::ResetLiveMigrateReceiveRequest>,
    ) -> Result<Response<controller_proto::ResetLiveMigrateReceiveResponse>, Status> {
        // Clearing can kill an in-flight receive, so it sits at the same bar as
        // draining a node rather than at `vm-admin` like MigrateVm.
        self.require_operator(&request, OperatorRole::ClusterAdmin)?;
        let actor = Self::audit_actor(&request);
        let req = request.into_inner();
        if req.node_id.trim().is_empty() {
            return Err(Status::invalid_argument(
                "node_id is required; name the node whose session you are clearing",
            ));
        }
        let vm = self.resolve_vm(&req.vm_id)?;
        let runtime_name = sanitize_nix_attr_key(&vm.name);
        let node = self
            .db
            .get_node(req.node_id.trim())
            .map_err(|e| Status::internal(e.to_string()))?
            .or_else(|| {
                self.db
                    .list_nodes()
                    .ok()
                    .and_then(|rows| rows.into_iter().find(|n| n.address == req.node_id.trim()))
            })
            .ok_or_else(|| Status::not_found(format!("node '{}' not found", req.node_id)))?;

        // A *receive* session only strands on a node that does not own the VM:
        // reassignment is the last step of a successful migration. On the
        // owning node the same cleanup would stop the VMM, delete the API
        // socket and unmap the RBD out from under a VM that is running fine.
        if node.id == vm.node_id {
            return Err(Status::failed_precondition(format!(
                "node '{}' currently owns VM '{}'; clearing a receive session there would stop \
                 the running VM. Inspect with `kctl get migrate-session {}` and reset only a \
                 node that does not own the VM",
                node.id, vm.name, vm.name
            )));
        }

        let observed = self.fetch_receive_state(&node, &runtime_name).await?;
        let summary = observed
            .as_ref()
            .map(|s| s.summary.clone())
            .unwrap_or_default();

        match plan_reset(observed.as_ref(), req.force) {
            // Idempotent: clearing an already-clean node is a success, so a
            // retried runbook step never turns into an error an operator has
            // to reason about.
            ResetPlan::NothingToClear => {
                return Ok(Response::new(
                    controller_proto::ResetLiveMigrateReceiveResponse {
                        success: true,
                        cleared: false,
                        message: format!(
                            "node {} has no receive session for '{}'; nothing to clear",
                            node.id, vm.name
                        ),
                        node_id: node.id,
                        runtime_name,
                        observed,
                    },
                ));
            }
            ResetPlan::ReportOnly => {
                return Ok(Response::new(
                    controller_proto::ResetLiveMigrateReceiveResponse {
                        success: true,
                        cleared: false,
                        message: format!(
                            "refusing to clear without force. Node {} reports: {summary}",
                            node.id
                        ),
                        node_id: node.id,
                        runtime_name,
                        observed,
                    },
                ));
            }
            ResetPlan::Clear => {}
        }

        // Reuse the abort path so the cleanup (receive task, VMM pid, port
        // reservation, marker, pid file, API socket, RBD unmap) lives in one
        // place, including its orphan reaping for a session lost to an agent
        // restart. The volume row is what lets the node unmap the image; a VM
        // without one simply skips that step.
        let volume = self
            .db
            .get_volume_by_vm(&vm.id)
            .map_err(|e| Status::internal(e.to_string()))?;
        let mut admin = self.ensure_admin_client_for_node(&node).await?;
        let aborted = admin
            .abort_live_migrate_receive(node_proto::AbortLiveMigrateReceiveRequest {
                vm_name: runtime_name.clone(),
                rbd_pool: volume.as_ref().map(|v| v.pool.clone()).unwrap_or_default(),
                rbd_image: volume.as_ref().map(|v| v.image.clone()).unwrap_or_default(),
                rbd_volumes: vec![],
            })
            .await
            .map_err(|e| {
                status_with_context(
                    &e,
                    &format!(
                        "clearing live-migrate receive session for '{}' on node {}",
                        vm.name, node.id
                    ),
                )
            })?
            .into_inner();

        // The node snapshots the session again inside the abort, immediately
        // before tearing it down. Prefer that over the read taken a moment
        // earlier: it is what was actually destroyed, and a receive can start
        // or die in between.
        let observed = aborted
            .observed
            .as_ref()
            .map(receive_state_from_node)
            .or(observed);
        let summary = observed
            .as_ref()
            .map(|s| s.summary.clone())
            .unwrap_or(summary);

        let killed_live_receive = observed.as_ref().is_some_and(receive_looks_live);
        if killed_live_receive {
            warn!(
                vm = %vm.name,
                node = %node.id,
                "cleared a live-migrate receive session that still looked active"
            );
        }
        let mut mirror_note = String::new();
        if crate::local_migrate::is_local_block(&vm.storage_backend) {
            // This node does not own the VM, so its copy is a stranded target.
            // The owner keeps its disk; only the mirror leg is removed.
            if let Err(e) = self.abort_volume_on(&node, &runtime_name, true).await {
                warn!(vm = %vm.name, node = %node.id, error = %e, "destination mirror cleanup");
                mirror_note = format!(" destination mirror cleanup failed: {e}");
            }
            if let Ok(Some(owner)) = self.db.get_node(&vm.node_id) {
                self.abort_volume_on(&owner, &runtime_name, false)
                    .await
                    .map_err(|e| {
                        status_with_context(
                            &e,
                            &format!(
                                "receive session on {} was cleared, but {} is still mirroring",
                                node.id, owner.id
                            ),
                        )
                    })?;
            }
        }
        self.record_audit(
            &actor,
            "ResetLiveMigrateReceive",
            &format!("vm/{}", vm.id),
            format!(
                "node={} live={} observed={}",
                node.id, killed_live_receive, summary
            ),
        );

        Ok(Response::new(
            controller_proto::ResetLiveMigrateReceiveResponse {
                success: true,
                cleared: true,
                message: format!(
                    "cleared live-migrate receive session for '{}' on node {}{} ({}){mirror_note}",
                    vm.name,
                    node.id,
                    if killed_live_receive {
                        " -- WARNING: it still looked active, so an in-flight migration was killed"
                    } else {
                        ""
                    },
                    aborted.message
                ),
                node_id: node.id,
                runtime_name,
                observed,
            },
        ))
    }

    async fn approve_node(
        &self,
        request: Request<controller_proto::ApproveNodeRequest>,
    ) -> Result<Response<controller_proto::ApproveNodeResponse>, Status> {
        self.require_operator(&request, OperatorRole::ClusterAdmin)?;
        let actor = Self::audit_actor(&request);
        let req = request.into_inner();

        let node = self
            .db
            .get_node(&req.node_id)
            .map_err(|e| Status::internal(e.to_string()))?
            .ok_or_else(|| Status::not_found(format!("node '{}' not found", req.node_id)))?;

        if node.approval_status == "approved" {
            return Ok(Response::new(controller_proto::ApproveNodeResponse {
                success: true,
                message: "node is already approved".into(),
            }));
        }

        self.db
            .set_node_approval(&req.node_id, "approved")
            .map_err(|e| Status::internal(format!("approving node: {e}")))?;
        self.db
            .update_node_status(&req.node_id, "ready")
            .map_err(|e| Status::internal(format!("updating node status: {e}")))?;

        if let Err(e) = self.clients.connect(&node.address).await {
            warn!(address = %node.address, error = %e, "failed to connect to approved node");
        }

        self.log_replication_event_required(
            &actor,
            Some("ApproveNode"),
            EVT_NODE_APPROVE,
            &format!("node/{}", req.node_id),
            serde_json::json!({
                "nodeId": req.node_id,
                "address": node.address,
            }),
        )?;

        info!(node_id = %req.node_id, "node approved");

        Ok(Response::new(controller_proto::ApproveNodeResponse {
            success: true,
            message: format!("node '{}' approved", req.node_id),
        }))
    }

    async fn reject_node(
        &self,
        request: Request<controller_proto::RejectNodeRequest>,
    ) -> Result<Response<controller_proto::RejectNodeResponse>, Status> {
        self.require_operator(&request, OperatorRole::ClusterAdmin)?;
        let actor = Self::audit_actor(&request);
        let req = request.into_inner();

        let _node = self
            .db
            .get_node(&req.node_id)
            .map_err(|e| Status::internal(e.to_string()))?
            .ok_or_else(|| Status::not_found(format!("node '{}' not found", req.node_id)))?;

        self.db
            .set_node_approval(&req.node_id, "rejected")
            .map_err(|e| Status::internal(format!("rejecting node: {e}")))?;
        self.db
            .update_node_status(&req.node_id, "rejected")
            .map_err(|e| Status::internal(format!("updating node status: {e}")))?;

        self.log_replication_event_required(
            &actor,
            Some("RejectNode"),
            EVT_NODE_REJECT,
            &format!("node/{}", req.node_id),
            serde_json::json!({
                "nodeId": req.node_id,
            }),
        )?;

        info!(node_id = %req.node_id, "node rejected");

        Ok(Response::new(controller_proto::RejectNodeResponse {
            success: true,
            message: format!("node '{}' rejected", req.node_id),
        }))
    }

    async fn cordon_node(
        &self,
        request: Request<controller_proto::CordonNodeRequest>,
    ) -> Result<Response<controller_proto::CordonNodeResponse>, Status> {
        self.require_operator(&request, OperatorRole::ClusterAdmin)?;
        let actor = Self::audit_actor(&request);
        let req = request.into_inner();
        let node = self
            .db
            .get_node(&req.node_id)
            .map_err(|e| Status::internal(e.to_string()))?
            .ok_or_else(|| Status::not_found(format!("node '{}' not found", req.node_id)))?;

        if matches!(node.status.as_str(), "cordoned" | "draining" | "drained") {
            return Ok(Response::new(controller_proto::CordonNodeResponse {
                success: true,
                message: format!(
                    "node '{}' is already unschedulable ({})",
                    node.id, node.status
                ),
            }));
        }

        self.db
            .update_node_status(&node.id, "cordoned")
            .map_err(|e| Status::internal(format!("cordoning node: {e}")))?;
        self.log_replication_event_required(
            &actor,
            Some("CordonNode"),
            EVT_NODE_CORDON,
            &format!("node/{}", node.id),
            serde_json::json!({ "nodeId": node.id }),
        )?;
        info!(node_id = %node.id, "node cordoned");
        Ok(Response::new(controller_proto::CordonNodeResponse {
            success: true,
            message: format!(
                "node '{}' cordoned; existing VMs stay, new placement will not use it",
                node.id
            ),
        }))
    }

    async fn uncordon_node(
        &self,
        request: Request<controller_proto::UncordonNodeRequest>,
    ) -> Result<Response<controller_proto::UncordonNodeResponse>, Status> {
        self.require_operator(&request, OperatorRole::ClusterAdmin)?;
        let actor = Self::audit_actor(&request);
        let req = request.into_inner();
        let node = self
            .db
            .get_node(&req.node_id)
            .map_err(|e| Status::internal(e.to_string()))?
            .ok_or_else(|| Status::not_found(format!("node '{}' not found", req.node_id)))?;

        if !matches!(node.status.as_str(), "cordoned" | "draining" | "drained") {
            return Ok(Response::new(controller_proto::UncordonNodeResponse {
                success: true,
                message: format!(
                    "node '{}' is not cordoned or drained ({})",
                    node.id, node.status
                ),
            }));
        }
        if node.approval_status != "approved" {
            return Err(Status::failed_precondition(format!(
                "node '{}' is not approved ({}); approve it before uncordoning",
                node.id, node.approval_status
            )));
        }
        if !req.force {
            if let Some(member) = self
                .db
                .get_slurm_cluster_node(&node.id)
                .map_err(|e| Status::internal(format!("reading slurm membership: {e}")))?
            {
                if member.roles.split(',').any(|role| role == "compute") {
                    return Err(Status::failed_precondition(format!(
                        "node '{}' is a compute member of SlurmCluster '{}'; pass force to uncordon it anyway",
                        node.id, member.cluster_name
                    )));
                }
            }
        }

        self.db
            .update_node_status(&node.id, "ready")
            .map_err(|e| Status::internal(format!("uncordoning node: {e}")))?;
        self.log_replication_event_required(
            &actor,
            Some("UncordonNode"),
            EVT_NODE_UNCORDON,
            &format!("node/{}", node.id),
            serde_json::json!({ "nodeId": node.id }),
        )?;
        info!(node_id = %node.id, "node uncordoned");
        Ok(Response::new(controller_proto::UncordonNodeResponse {
            success: true,
            message: format!("node '{}' is schedulable again", node.id),
        }))
    }

    async fn delete_node(
        &self,
        request: Request<controller_proto::DeleteNodeRequest>,
    ) -> Result<Response<controller_proto::DeleteNodeResponse>, Status> {
        self.require_operator(&request, OperatorRole::ClusterAdmin)?;
        let actor = Self::audit_actor(&request);
        let req = request.into_inner();
        let node = self
            .db
            .get_node(&req.node_id)
            .map_err(|e| Status::internal(e.to_string()))?
            .ok_or_else(|| Status::not_found(format!("node '{}' not found", req.node_id)))?;

        let vms = self
            .db
            .list_vms_for_node(&node.id)
            .map_err(|e| Status::internal(format!("listing VMs: {e}")))?;
        if !vms.is_empty() {
            return Err(Status::failed_precondition(format!(
                "node '{}' still hosts {} VM(s); drain them before deleting the node",
                node.id,
                vms.len()
            )));
        }
        let networks = self
            .db
            .list_networks_for_node(&node.id)
            .map_err(|e| Status::internal(format!("listing networks: {e}")))?;
        if !networks.is_empty() {
            return Err(Status::failed_precondition(format!(
                "node '{}' still has {} network(s); delete them before deleting the node",
                node.id,
                networks.len()
            )));
        }
        let workloads = self
            .db
            .count_workloads_for_node(&node.id)
            .map_err(|e| Status::internal(format!("listing workloads: {e}")))?;
        if workloads > 0 {
            return Err(Status::failed_precondition(format!(
                "node '{}' still hosts {workloads} workload(s); remove them before deleting the node",
                node.id
            )));
        }

        self.db
            .update_node_status(&node.id, "cordoned")
            .map_err(|e| Status::internal(format!("cordoning node: {e}")))?;

        let revoked = self.revoke_node_certificates(&node.id)?;

        let removed = self.db.delete_node(&node.id).map_err(|e| {
            let msg = e.to_string();
            if msg.contains("FOREIGN KEY") {
                Status::failed_precondition(format!(
                    "node '{}' still has dependent records; remove them before deleting the node",
                    node.id
                ))
            } else {
                Status::internal(format!("deleting node: {e}"))
            }
        })?;
        if !removed {
            return Err(Status::not_found(format!("node '{}' not found", node.id)));
        }

        self.log_replication_event_required(
            &actor,
            Some("DeleteNode"),
            EVT_NODE_DELETE,
            &format!("node/{}", node.id),
            serde_json::json!({
                "nodeId": node.id,
                "certificatesRevoked": revoked,
            }),
        )?;
        info!(node_id = %node.id, certificates_revoked = revoked, "node deleted");
        Ok(Response::new(controller_proto::DeleteNodeResponse {
            success: true,
            message: format!(
                "node '{}' deleted; {revoked} certificate(s) revoked",
                node.id
            ),
            certificates_revoked: revoked,
        }))
    }

    async fn get_cluster_health(
        &self,
        request: Request<controller_proto::GetClusterHealthRequest>,
    ) -> Result<Response<controller_proto::GetClusterHealthResponse>, Status> {
        self.require_operator(&request, OperatorRole::ReadOnly)?;
        let _ = request;
        let nodes = self.db.list_nodes().map_err(internal_db)?;
        let vms = self.db.list_vms().map_err(internal_db)?;
        let conflicts = self
            .db
            .count_unresolved_replication_conflicts()
            .map_err(internal_db)?;
        let now = crate::pki::format_ts(time::OffsetDateTime::now_utc());
        let warn = crate::pki::format_ts(
            time::OffsetDateTime::now_utc()
                + time::Duration::days(crate::cluster_health::CERT_EXPIRY_WARN_DAYS as i64),
        );
        let inventory_expiring = self
            .db
            .count_certificates(&now, &warn)
            .map(|counts| counts.4)
            .unwrap_or(0);
        let mut health = crate::cluster_health::assess(&nodes, &vms, conflicts, inventory_expiring);
        let report =
            self.admission_report(None, None)
                .unwrap_or_else(|_| crate::ha::AdmissionReport {
                    capacity: crate::ha::CAPACITY_NOT_APPLICABLE.to_string(),
                    host_failures_tolerated: 0,
                    memory_headroom_bytes: 0,
                    cpu_headroom: 0,
                    message: "admission control could not be computed".into(),
                });
        let lost = self.db.list_lost_node_ids().unwrap_or_default();
        let policies = self.db.list_vm_ha().unwrap_or_default();
        let unprotected_vms_on_lost_nodes = vms
            .iter()
            .filter(|vm| lost.iter().any(|id| id == &vm.node_id))
            .filter(|vm| {
                let replica = policies
                    .iter()
                    .find(|row| row.vm_id == vm.id)
                    .map(|row| row.replica_node.as_str())
                    .unwrap_or("");
                !crate::ha::is_protected(&vm.storage_backend, replica)
            })
            .count() as i32;
        if unprotected_vms_on_lost_nodes > 0 {
            health.conditions.push(format!(
                "{unprotected_vms_on_lost_nodes} unprotected VM(s) are on a lost node"
            ));
            if health.status == crate::cluster_health::STATUS_HEALTHY {
                health.status = crate::cluster_health::STATUS_DEGRADED.to_string();
            }
        }
        let lease = self.db.get_cluster_lease("failover").ok().flatten();
        let ha_lease_holder = lease
            .as_ref()
            .map(|row| row.holder.clone())
            .unwrap_or_default();
        let ha_lease_term = lease.as_ref().map(|row| row.term).unwrap_or(0);
        Ok(Response::new(controller_proto::GetClusterHealthResponse {
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
            host_failures_tolerated: report.host_failures_tolerated,
            failover_capacity: report.capacity,
            failover_memory_headroom_bytes: report.memory_headroom_bytes,
            ha_lease_holder,
            ha_lease_term,
            unprotected_vms_on_lost_nodes,
        }))
    }

    async fn get_ha_status(
        &self,
        request: Request<controller_proto::GetHaStatusRequest>,
    ) -> Result<Response<controller_proto::GetHaStatusResponse>, Status> {
        self.require_operator(&request, OperatorRole::ReadOnly)?;
        let _ = request;
        let report =
            self.admission_report(None, None)
                .unwrap_or_else(|_| crate::ha::AdmissionReport {
                    capacity: crate::ha::CAPACITY_NOT_APPLICABLE.to_string(),
                    host_failures_tolerated: 0,
                    memory_headroom_bytes: 0,
                    cpu_headroom: 0,
                    message: String::new(),
                });
        let lease = self.db.get_cluster_lease("failover").ok().flatten();
        let controller_id = self
            .replication
            .as_ref()
            .map(|rep| rep.controller_id.clone())
            .unwrap_or_else(|| "local".to_string());
        Ok(Response::new(controller_proto::GetHaStatusResponse {
            enabled: self.failover.enabled,
            acting: self.failover_may_act(),
            lease_holder: lease
                .as_ref()
                .map(|row| row.holder.clone())
                .unwrap_or_default(),
            lease_term: lease.as_ref().map(|row| row.term).unwrap_or(0),
            lease_expires_at_ms: lease.as_ref().map(|row| row.expires_at_ms).unwrap_or(0),
            node_timeout_secs: i32::try_from(self.failover.node_timeout_secs).unwrap_or(30),
            detect_interval_secs: i32::try_from(self.failover.detect_interval_secs).unwrap_or(5),
            isolation_response: self.failover.isolation_response.clone(),
            host_failures_to_tolerate: self.failover.admission_control.host_failures_to_tolerate,
            failover_capacity: report.capacity,
            memory_headroom_bytes: report.memory_headroom_bytes,
            controller_id,
        }))
    }

    async fn list_ha_events(
        &self,
        request: Request<controller_proto::ListHaEventsRequest>,
    ) -> Result<Response<controller_proto::ListHaEventsResponse>, Status> {
        self.require_operator(&request, OperatorRole::ReadOnly)?;
        let req = request.into_inner();
        let limit = if req.limit <= 0 {
            50
        } else {
            i64::from(req.limit)
        };
        let rows = self
            .db
            .list_ha_events(&req.node_id, &req.vm_id, limit)
            .map_err(|e| Status::internal(e.to_string()))?;
        Ok(Response::new(controller_proto::ListHaEventsResponse {
            events: rows
                .into_iter()
                .map(|row| controller_proto::HaEvent {
                    id: row.id,
                    node_id: row.node_id,
                    vm_id: row.vm_id,
                    phase: row.phase,
                    detail: row.detail,
                    created_at: row.created_at,
                })
                .collect(),
        }))
    }

    async fn backup_cluster(
        &self,
        request: Request<controller_proto::BackupClusterRequest>,
    ) -> Result<Response<controller_proto::BackupClusterResponse>, Status> {
        self.require_operator(&request, OperatorRole::ClusterAdmin)?;
        let actor = Self::audit_actor(&request);
        let _ = request;
        let (sqlite, schema_version) = self
            .db
            .snapshot_sqlite()
            .map_err(|e| Status::internal(format!("backup: {e}")))?;
        self.record_audit(
            &actor,
            "BackupCluster",
            "cluster/database",
            format!("schema {schema_version}, {} bytes", sqlite.len()),
        );
        Ok(Response::new(controller_proto::BackupClusterResponse {
            sqlite,
            schema_version,
        }))
    }

    async fn restore_cluster(
        &self,
        request: Request<controller_proto::RestoreClusterRequest>,
    ) -> Result<Response<controller_proto::RestoreClusterResponse>, Status> {
        self.require_operator(&request, OperatorRole::ClusterAdmin)?;
        let actor = Self::audit_actor(&request);
        let req = request.into_inner();
        if !req.confirm {
            return Err(Status::failed_precondition(
                "restore replaces the controller database; set confirm=true",
            ));
        }
        let schema_version = self.db.restore_sqlite(&req.sqlite).map_err(|e| {
            if e.contains("not a SQLite") || e.contains("schema version") || e.contains("empty") {
                Status::invalid_argument(e)
            } else {
                Status::internal(format!("restore: {e}"))
            }
        })?;
        if let Err(error) = self.pki.revocation.refresh(&self.db) {
            warn!(%error, "database restored but the revocation cache did not refresh");
        }
        self.record_audit(
            &actor,
            "RestoreCluster",
            "cluster/database",
            format!("restored schema {schema_version}"),
        );
        info!(schema_version, "controller database restored from snapshot");
        Ok(Response::new(controller_proto::RestoreClusterResponse {
            success: true,
            message: format!(
                "restored controller database at schema {schema_version}; restart peer controllers so they do not keep the previous copy"
            ),
            schema_version,
        }))
    }

    async fn renew_node_cert(
        &self,
        request: Request<controller_proto::RenewNodeCertRequest>,
    ) -> Result<Response<controller_proto::RenewNodeCertResponse>, Status> {
        auth::require_peer(&request, &[CN_NODE_PREFIX])?;
        let req = request.into_inner();

        let node = self
            .db
            .get_node(&req.node_id)
            .map_err(|e| Status::internal(e.to_string()))?
            .ok_or_else(|| Status::not_found(format!("node '{}' not found", req.node_id)))?;

        if node.approval_status != "approved" {
            return Err(Status::permission_denied(format!(
                "node '{}' is not approved (status: {})",
                req.node_id, node.approval_status
            )));
        }

        let sub_ca = self
            .sub_ca
            .lock()
            .map_err(|_| Status::internal("sub-CA lock poisoned"))?
            .clone();

        if !sub_ca.is_available() {
            return Err(Status::unavailable(
                "sub-CA is not configured on this controller; certificate renewal is unavailable",
            ));
        }

        let node_host = node.address.split(':').next().unwrap_or("").to_string();
        if node_host.is_empty() {
            return Err(Status::internal(format!(
                "cannot determine host from node address '{}'",
                node.address
            )));
        }

        let (chain_pem, key_pem) =
            signing::sign_node_cert(&sub_ca.cert_pem, &sub_ca.key_pem, &node_host)
                .map_err(|e| Status::internal(format!("signing node cert: {e}")))?;

        self.record_issued_cert(&chain_pem, &req.node_id);
        info!(node_id = %req.node_id, host = %node_host, "renewed node certificate via sub-CA");

        Ok(Response::new(controller_proto::RenewNodeCertResponse {
            success: true,
            cert_pem: chain_pem,
            key_pem,
            message: format!("certificate renewed for node '{}'", req.node_id),
        }))
    }

    async fn issue_node_bootstrap_cert(
        &self,
        request: Request<controller_proto::IssueNodeBootstrapCertRequest>,
    ) -> Result<Response<controller_proto::IssueNodeBootstrapCertResponse>, Status> {
        self.require_operator(&request, OperatorRole::ClusterAdmin)?;
        let req = request.into_inner();

        let node_id = req.node_id.trim();
        if node_id.is_empty() {
            return Err(Status::invalid_argument("node_id is required"));
        }
        let node_host = req.node_host.trim();
        if node_host.is_empty() {
            return Err(Status::invalid_argument("node_host is required"));
        }

        let sub_ca = self
            .sub_ca
            .lock()
            .map_err(|_| Status::internal("sub-CA lock poisoned"))?
            .clone();

        if !sub_ca.is_available() {
            return Err(Status::unavailable(
                "sub-CA is not configured on this controller; node bootstrap certificate issuance is unavailable",
            ));
        }

        let (chain_pem, key_pem) =
            signing::sign_node_cert(&sub_ca.cert_pem, &sub_ca.key_pem, node_host)
                .map_err(|e| Status::internal(format!("signing bootstrap node cert: {e}")))?;

        self.record_issued_cert(&chain_pem, node_id);
        info!(node_id = %node_id, node_host = %node_host, "issued bootstrap node certificate via sub-CA");

        Ok(Response::new(
            controller_proto::IssueNodeBootstrapCertResponse {
                success: true,
                cert_pem: chain_pem,
                key_pem,
                message: format!("bootstrap certificate issued for node '{}'", node_id),
            },
        ))
    }

    async fn rotate_sub_ca(
        &self,
        request: Request<controller_proto::RotateSubCaRequest>,
    ) -> Result<Response<controller_proto::RotateSubCaResponse>, Status> {
        self.require_operator(&request, OperatorRole::ClusterAdmin)?;
        let actor = Self::audit_actor(&request);
        let req = request.into_inner();

        if req.sub_ca_cert_pem.trim().is_empty() || req.sub_ca_key_pem.trim().is_empty() {
            return Err(Status::invalid_argument(
                "sub_ca_cert_pem and sub_ca_key_pem are required",
            ));
        }

        signing::validate_sub_ca_cert(&req.sub_ca_cert_pem)
            .map_err(|e| Status::invalid_argument(format!("invalid sub-CA cert: {e}")))?;

        let mut sub_ca = self
            .sub_ca
            .lock()
            .map_err(|_| Status::internal("sub-CA lock poisoned"))?;

        if let Some(cert_file) = &sub_ca.cert_file {
            std::fs::write(cert_file, &req.sub_ca_cert_pem)
                .map_err(|e| Status::internal(format!("writing sub-CA cert: {e}")))?;
        }
        if let Some(key_file) = &sub_ca.key_file {
            std::fs::write(key_file, &req.sub_ca_key_pem)
                .map_err(|e| Status::internal(format!("writing sub-CA key: {e}")))?;
        }

        sub_ca.cert_pem = req.sub_ca_cert_pem;
        sub_ca.key_pem = req.sub_ca_key_pem;

        info!("sub-CA rotated via kctl");
        self.record_audit(&actor, "RotateSubCa", "sub-ca", "");

        Ok(Response::new(controller_proto::RotateSubCaResponse {
            success: true,
            message: "sub-CA rotated successfully".into(),
        }))
    }

    async fn reload_tls(
        &self,
        request: Request<controller_proto::ReloadTlsRequest>,
    ) -> Result<Response<controller_proto::ReloadTlsResponse>, Status> {
        self.require_operator(&request, OperatorRole::ClusterAdmin)?;
        let actor = Self::audit_actor(&request);
        let req = request.into_inner();

        if req.cert_pem.trim().is_empty() || req.key_pem.trim().is_empty() {
            return Err(Status::invalid_argument(
                "cert_pem and key_pem are required",
            ));
        }

        let tls = self.tls_paths.as_ref().ok_or_else(|| {
            Status::failed_precondition("TLS is not configured on this controller")
        })?;

        std::fs::write(&tls.cert_file, &req.cert_pem)
            .map_err(|e| Status::internal(format!("writing cert: {e}")))?;
        std::fs::write(&tls.key_file, &req.key_pem)
            .map_err(|e| Status::internal(format!("writing key: {e}")))?;

        info!(
            cert = %tls.cert_file,
            key = %tls.key_file,
            "controller TLS cert written to disk"
        );

        #[cfg(unix)]
        {
            let pid = std::process::id();
            unsafe {
                libc::kill(pid as libc::pid_t, libc::SIGHUP);
            }
            info!("SIGHUP sent to self, TLS reload in progress");
        }

        self.record_audit(&actor, "ReloadTls", "controller-tls", "");

        Ok(Response::new(controller_proto::ReloadTlsResponse {
            success: true,
            message: "TLS certificate updated; server reloading".into(),
        }))
    }

    async fn sign_node_csr(
        &self,
        request: Request<controller_proto::SignNodeCsrRequest>,
    ) -> Result<Response<controller_proto::SignNodeCsrResponse>, Status> {
        auth::require_peer(&request, &[CN_NODE_PREFIX])?;
        let req = request.into_inner();

        let node = self
            .db
            .get_node(&req.node_id)
            .map_err(internal_db)?
            .ok_or_else(|| Status::not_found(format!("node '{}' not found", req.node_id)))?;
        if node.approval_status != "approved" {
            return Err(Status::permission_denied(format!(
                "node '{}' is not approved (status: {})",
                req.node_id, node.approval_status
            )));
        }
        if req.csr_pem.trim().is_empty() {
            return Err(Status::invalid_argument("csr_pem is required"));
        }

        let sub_ca = self.sub_ca_snapshot()?;
        if !sub_ca.is_available() {
            return Err(Status::unavailable(
                "sub-CA is not configured on this controller; certificate rotation is unavailable",
            ));
        }

        // The SAN and CN are derived from the address the node registered
        // with, not from anything in the CSR.
        let node_host = node.address.split(':').next().unwrap_or("").to_string();
        if node_host.is_empty() {
            return Err(Status::internal(format!(
                "cannot determine host from node address '{}'",
                node.address
            )));
        }

        let chain_pem = signing::sign_node_csr(
            &sub_ca.cert_pem,
            &sub_ca.key_pem,
            &req.csr_pem,
            &node_host,
            self.pki.rotation.cert_validity_days,
        )
        .map_err(|e| Status::invalid_argument(format!("signing node CSR: {e}")))?;

        let meta = crate::pki::inventory::record_signed_chain(&self.db, &chain_pem, &req.node_id)
            .map_err(|e| Status::internal(format!("recording issued certificate: {e}")))?;

        info!(
            node_id = %req.node_id,
            host = %node_host,
            serial = %meta.serial_hex,
            "signed node CSR; private key stayed on the node"
        );
        self.record_audit(
            &format!("{}{}", CN_NODE_PREFIX, node_host),
            "SignNodeCsr",
            &format!("node/{}", req.node_id),
            &meta.serial_hex,
        );

        Ok(Response::new(controller_proto::SignNodeCsrResponse {
            success: true,
            cert_chain_pem: chain_pem,
            serial_hex: meta.serial_hex,
            not_after: Some(prost_timestamp(meta.not_after)),
            message: format!("certificate issued for node '{}'", req.node_id),
        }))
    }

    async fn rotate_node_certs(
        &self,
        request: Request<controller_proto::RotateNodeCertsRequest>,
    ) -> Result<Response<controller_proto::RotateNodeCertsResponse>, Status> {
        self.require_operator(&request, OperatorRole::ClusterAdmin)?;
        let actor = Self::audit_actor(&request);
        let req = request.into_inner();

        let targets: Vec<String> = if req.all_nodes {
            self.db
                .list_nodes()
                .map_err(internal_db)?
                .into_iter()
                .filter(|n| n.approval_status == "approved")
                .map(|n| n.id)
                .collect()
        } else {
            let node_id = req.node_id.trim();
            if node_id.is_empty() {
                return Err(Status::invalid_argument(
                    "node_id is required unless all_nodes is set",
                ));
            }
            vec![node_id.to_string()]
        };
        if targets.is_empty() {
            return Ok(Response::new(controller_proto::RotateNodeCertsResponse {
                success: true,
                results: Vec::new(),
                message: "no approved nodes to rotate".into(),
            }));
        }

        let mut results = Vec::with_capacity(targets.len());
        let mut failures = 0;
        for node_id in targets {
            // `force` so an on-demand rotation happens even outside the
            // renewal window — that is the whole point of asking.
            match crate::cert_rotation_reconciler::rotate_node(
                &self.db,
                &self.clients,
                &node_id,
                true,
            )
            .await
            {
                Ok(resp) => results.push(controller_proto::NodeCertRotationResult {
                    node_id,
                    success: true,
                    serial_hex: resp.serial_hex,
                    message: resp.message,
                }),
                Err(error) => {
                    failures += 1;
                    warn!(node_id = %node_id, %error, "operator-triggered rotation failed");
                    results.push(controller_proto::NodeCertRotationResult {
                        node_id,
                        success: false,
                        serial_hex: String::new(),
                        message: error,
                    })
                }
            }
        }

        let total = results.len();
        self.record_audit(
            &actor,
            "RotateNodeCerts",
            "certificates",
            format!("{}/{} rotated", total - failures, total),
        );
        Ok(Response::new(controller_proto::RotateNodeCertsResponse {
            success: failures == 0,
            results,
            message: format!("{} of {total} nodes rotated", total - failures),
        }))
    }

    async fn list_certificates(
        &self,
        request: Request<controller_proto::ListCertificatesRequest>,
    ) -> Result<Response<controller_proto::ListCertificatesResponse>, Status> {
        self.require_operator(&request, OperatorRole::ReadOnly)?;
        let req = request.into_inner();

        let status = cert_status_to_db_str(req.status);
        let expiring_before = if req.expiring_within_days > 0 {
            crate::pki::format_ts(
                time::OffsetDateTime::now_utc()
                    + time::Duration::days(i64::from(req.expiring_within_days)),
            )
        } else {
            String::new()
        };

        let rows = self
            .db
            .list_issued_certificates(status, req.node_id.trim(), &expiring_before)
            .map_err(internal_db)?;
        let now = time::OffsetDateTime::now_utc();
        Ok(Response::new(controller_proto::ListCertificatesResponse {
            certificates: rows.iter().map(|r| cert_info_from_row(r, now)).collect(),
        }))
    }

    async fn revoke_certificate(
        &self,
        request: Request<controller_proto::RevokeCertificateRequest>,
    ) -> Result<Response<controller_proto::RevokeCertificateResponse>, Status> {
        self.require_operator(&request, OperatorRole::ClusterAdmin)?;
        let actor = Self::audit_actor(&request);
        let req = request.into_inner();

        let reason = req.reason;
        if !crate::pki::is_valid_reason_code(reason) {
            return Err(Status::invalid_argument(format!(
                "revocation reason {reason} is not an RFC 5280 reason code"
            )));
        }

        let serials = self.resolve_revocation_targets(&req)?;
        if serials.is_empty() {
            return Err(Status::not_found(
                "no matching certificate found in the inventory",
            ));
        }

        let revoked_at = crate::pki::format_ts(time::OffsetDateTime::now_utc());
        let mut revoked = Vec::with_capacity(serials.len());
        for serial in &serials {
            let row = self
                .db
                .revoke_certificate_by_serial(serial, reason, &revoked_at)
                .map_err(internal_db)?;
            if let Some(row) = row {
                // Take effect on the very next RPC rather than at the next
                // reconciler tick.
                self.pki.revocation.insert_revoked(&row.serial_hex);
                revoked.push(row);
            }
        }

        // Roll a new CRL immediately so nodes and external tooling observe the
        // revocation without waiting for the periodic regeneration.
        let sub_ca = self.sub_ca_snapshot()?;
        let crl_number = match crate::pki::crl::ensure_current(
            &self.db,
            &sub_ca,
            &self.pki.crl_cache,
            time::Duration::hours(self.pki.pki.crl_validity_hours),
            time::Duration::hours(self.pki.pki.crl_refresh_before_hours),
            true,
        ) {
            Ok(Some(crl)) => crl.crl_number,
            Ok(None) => 0,
            Err(error) => {
                warn!(%error, "revocation recorded but CRL regeneration failed");
                0
            }
        };

        for row in &revoked {
            info!(
                serial = %row.serial_hex,
                subject = %row.subject_cn,
                node_id = %row.node_id,
                reason,
                "certificate revoked"
            );
            self.record_audit(
                &actor,
                "RevokeCertificate",
                &format!("certificate/{}", row.serial_hex),
                format!("reason={reason}"),
            );
        }

        let now = time::OffsetDateTime::now_utc();
        let count = revoked.len();
        Ok(Response::new(controller_proto::RevokeCertificateResponse {
            success: true,
            revoked: revoked.iter().map(|r| cert_info_from_row(r, now)).collect(),
            crl_number,
            message: format!("{count} certificate(s) revoked"),
        }))
    }

    async fn get_crl(
        &self,
        request: Request<controller_proto::GetCrlRequest>,
    ) -> Result<Response<controller_proto::GetCrlResponse>, Status> {
        // Nodes fetch the CRL over their existing mTLS channel, so both node
        // and operator identities are accepted here.
        if auth::peer_cn(&request)
            .map(|cn| cn.starts_with(CN_NODE_PREFIX))
            .unwrap_or(false)
        {
            auth::require_peer(&request, &[CN_NODE_PREFIX])?;
        } else {
            self.require_operator(&request, OperatorRole::ReadOnly)?;
        }

        let sub_ca = self.sub_ca_snapshot()?;
        let crl = crate::pki::crl::ensure_current(
            &self.db,
            &sub_ca,
            &self.pki.crl_cache,
            time::Duration::hours(self.pki.pki.crl_validity_hours),
            time::Duration::hours(self.pki.pki.crl_refresh_before_hours),
            false,
        )
        .map_err(|e| Status::internal(format!("generating CRL: {e}")))?;

        match crl {
            Some(crl) => Ok(Response::new(controller_proto::GetCrlResponse {
                success: true,
                crl_pem: crl.pem,
                crl_der: crl.der,
                crl_number: crl.crl_number,
                this_update: Some(prost_timestamp(crl.this_update)),
                next_update: Some(prost_timestamp(crl.next_update)),
                revoked_count: crl.revoked_count,
                message: String::new(),
            })),
            None => Ok(Response::new(controller_proto::GetCrlResponse {
                success: false,
                message: "no sub-CA is configured on this controller; no CRL can be signed".into(),
                ..Default::default()
            })),
        }
    }

    async fn get_pki_status(
        &self,
        request: Request<controller_proto::GetPkiStatusRequest>,
    ) -> Result<Response<controller_proto::GetPkiStatusResponse>, Status> {
        self.require_operator(&request, OperatorRole::ReadOnly)?;

        let now = time::OffsetDateTime::now_utc();
        let warn_threshold =
            crate::pki::format_ts(now + time::Duration::days(self.pki.rotation.warn_before_days));
        let (active, rotated, revoked, expired, expiring_soon) = self
            .db
            .count_certificates(&crate::pki::format_ts(now), &warn_threshold)
            .map_err(internal_db)?;

        let soonest = self
            .db
            .list_issued_certificates(crate::db::CERT_STATUS_ACTIVE, "", "")
            .map_err(internal_db)?
            .iter()
            .take(20)
            .map(|r| cert_info_from_row(r, now))
            .collect();

        let sub_ca = self.sub_ca_snapshot()?;
        let sub_ca_not_after = if sub_ca.is_available() {
            crate::pki::inventory::metadata_from_pem(&sub_ca.cert_pem)
                .ok()
                .map(|m| prost_timestamp(m.not_after))
        } else {
            None
        };
        let crl = self.pki.crl_cache.get();

        Ok(Response::new(controller_proto::GetPkiStatusResponse {
            active_count: active,
            rotated_count: rotated,
            revoked_count: revoked,
            expired_count: expired,
            expiring_soon_count: expiring_soon,
            warn_before_days: self.pki.rotation.warn_before_days as i32,
            renew_before_days: self.pki.rotation.renew_before_days as i32,
            rotation_enabled: self.pki.rotation.enabled,
            crl_number: crl.as_ref().map(|c| c.crl_number).unwrap_or(0),
            crl_this_update: crl.as_ref().map(|c| prost_timestamp(c.this_update)),
            crl_next_update: crl.as_ref().map(|c| prost_timestamp(c.next_update)),
            crl_available: crl.is_some(),
            sub_ca_available: sub_ca.is_available(),
            sub_ca_not_after,
            revocation_fail_mode: self.pki.revocation.fail_mode().as_str().to_string(),
            pki_http_base_url: self.pki.pki.base_url(),
            soonest_expiring: soonest,
        }))
    }

    async fn get_network_overview(
        &self,
        request: Request<controller_proto::GetNetworkOverviewRequest>,
    ) -> Result<Response<controller_proto::GetNetworkOverviewResponse>, Status> {
        self.require_operator(&request, OperatorRole::ReadOnly)?;

        let node_rows = self
            .db
            .list_nodes()
            .map_err(|e| Status::internal(e.to_string()))?;

        let approved: Vec<_> = node_rows
            .into_iter()
            .filter(|n| n.approval_status == "approved")
            .collect();

        let mut nodes = Vec::with_capacity(approved.len());
        for node in &approved {
            if node.status != "ready" {
                nodes.push(controller_proto::NodeNetworkInfo {
                    node_id: node.id.clone(),
                    hostname: node.hostname.clone(),
                    address: node.address.clone(),
                    gateway_interface: node.gateway_interface.clone(),
                    disable_vxlan: node.disable_vxlan,
                    interfaces: Vec::new(),
                });
                continue;
            }
            let interfaces = match self.ensure_admin_client_for_node(node).await {
                Ok(mut admin) => {
                    match admin
                        .list_network_interfaces(node_proto::ListNetworkInterfacesRequest {})
                        .await
                    {
                        Ok(resp) => resp
                            .into_inner()
                            .interfaces
                            .into_iter()
                            .map(|iface| controller_proto::NetworkInterfaceDetail {
                                name: iface.name,
                                mac_address: iface.mac_address,
                                state: iface.state,
                                mtu: iface.mtu,
                                addresses: iface.addresses,
                            })
                            .collect(),
                        Err(e) => {
                            warn!(node_id = %node.id, error = %e, "ListNetworkInterfaces failed");
                            vec![]
                        }
                    }
                }
                Err(e) => {
                    warn!(node_id = %node.id, error = %e, "cannot reach node for network overview");
                    vec![]
                }
            };

            nodes.push(controller_proto::NodeNetworkInfo {
                node_id: node.id.clone(),
                hostname: node.hostname.clone(),
                address: node.address.clone(),
                gateway_interface: node.gateway_interface.clone(),
                disable_vxlan: node.disable_vxlan,
                interfaces,
            });
        }

        Ok(Response::new(
            controller_proto::GetNetworkOverviewResponse {
                default_gateway_interface: self.default_network.gateway_interface.clone(),
                default_external_ip: self.default_network.external_ip.clone(),
                default_gateway_ip: self.default_network.gateway_ip.clone(),
                default_internal_netmask: self.default_network.internal_netmask.clone(),
                nodes,
            },
        ))
    }

    async fn get_storage_overview(
        &self,
        request: Request<controller_proto::GetStorageOverviewRequest>,
    ) -> Result<Response<controller_proto::GetStorageOverviewResponse>, Status> {
        self.require_operator(&request, OperatorRole::ReadOnly)?;
        let _ = request.into_inner();

        let node_rows = self
            .db
            .list_nodes()
            .map_err(|e| Status::internal(e.to_string()))?;
        let approved: Vec<_> = node_rows
            .into_iter()
            .filter(|n| n.approval_status == "approved")
            .collect();

        let (nodes_luks_tpm2, nodes_luks_keyfile, nodes_luks_unknown) = self
            .db
            .count_nodes_luks_method()
            .map_err(|e| Status::internal(e.to_string()))?;

        let mut backend_filesystem_nodes: i32 = 0;
        let mut backend_lvm_nodes: i32 = 0;
        let mut backend_zfs_nodes: i32 = 0;
        let mut backend_ceph_nodes: i32 = 0;
        let mut backend_unspecified_nodes: i32 = 0;
        for n in &approved {
            match n.storage_backend.as_str() {
                "filesystem" => backend_filesystem_nodes += 1,
                "lvm" => backend_lvm_nodes += 1,
                "zfs" => backend_zfs_nodes += 1,
                "ceph" => backend_ceph_nodes += 1,
                _ => backend_unspecified_nodes += 1,
            }
        }

        let mut nodes_out = Vec::with_capacity(approved.len());
        let mut nodes_disk_inventory_ok: i32 = 0;
        let mut total_block_devices: i32 = 0;

        for node in &approved {
            if node.status != "ready" {
                nodes_out.push(controller_proto::NodeStorageOverview {
                    node_id: node.id.clone(),
                    hostname: node.hostname.clone(),
                    address: node.address.clone(),
                    storage_backend: storage_backend_to_proto(&node.storage_backend),
                    luks_method: node.luks_method.clone(),
                    disk_inventory_ok: false,
                    disks: Vec::new(),
                    lvm_inventory_ok: false,
                    lvm_volume_groups: Vec::new(),
                    lvm_logical_volumes: Vec::new(),
                    lvm_physical_volumes: Vec::new(),
                });
                continue;
            }
            let (
                disks,
                disk_inventory_ok,
                lvm_inventory_ok,
                lvm_volume_groups,
                lvm_logical_volumes,
                lvm_physical_volumes,
            ) = match self.ensure_admin_client_for_node(node).await {
                Ok(mut admin) => {
                    let (disks, disk_inventory_ok) =
                        match admin.list_disks(node_proto::ListDisksRequest {}).await {
                            Ok(resp) => {
                                let disks: Vec<controller_proto::StorageDiskDetail> = resp
                                    .into_inner()
                                    .disks
                                    .into_iter()
                                    .map(|d| controller_proto::StorageDiskDetail {
                                        name: d.name,
                                        path: d.path,
                                        size: d.size,
                                        model: d.model,
                                        fstype: d.fstype,
                                        mountpoint: d.mountpoint,
                                    })
                                    .collect();
                                (disks, true)
                            }
                            Err(e) => {
                                warn!(
                                    node_id = %node.id,
                                    error = %e,
                                    "ListDisks failed for storage overview"
                                );
                                (vec![], false)
                            }
                        };

                    let (
                        lvm_inventory_ok,
                        lvm_volume_groups,
                        lvm_logical_volumes,
                        lvm_physical_volumes,
                    ) = match admin.get_lvm_info(node_proto::GetLvmInfoRequest {}).await {
                        Ok(resp) => {
                            let inner = resp.into_inner();
                            if inner.available {
                                (
                                    true,
                                    inner
                                        .volume_groups
                                        .into_iter()
                                        .map(|vg| controller_proto::StorageLvmVolumeGroupDetail {
                                            name: vg.name,
                                            size_bytes: vg.size_bytes,
                                            free_bytes: vg.free_bytes,
                                            attr: vg.attr,
                                        })
                                        .collect(),
                                    inner
                                        .logical_volumes
                                        .into_iter()
                                        .map(|lv| controller_proto::StorageLvmLogicalVolumeDetail {
                                            name: lv.name,
                                            vg_name: lv.vg_name,
                                            size_bytes: lv.size_bytes,
                                            attr: lv.attr,
                                            path: lv.path,
                                            pool: lv.pool,
                                            origin: lv.origin,
                                            data_percent: lv.data_percent,
                                            metadata_percent: lv.metadata_percent,
                                        })
                                        .collect(),
                                    inner
                                        .physical_volumes
                                        .into_iter()
                                        .map(|pv| {
                                            controller_proto::StorageLvmPhysicalVolumeDetail {
                                                name: pv.name,
                                                vg_name: pv.vg_name,
                                                size_bytes: pv.size_bytes,
                                                free_bytes: pv.free_bytes,
                                                attr: pv.attr,
                                            }
                                        })
                                        .collect(),
                                )
                            } else {
                                (false, vec![], vec![], vec![])
                            }
                        }
                        Err(e) => {
                            warn!(
                                node_id = %node.id,
                                error = %e,
                                "GetLvmInfo failed for storage overview"
                            );
                            (false, vec![], vec![], vec![])
                        }
                    };

                    (
                        disks,
                        disk_inventory_ok,
                        lvm_inventory_ok,
                        lvm_volume_groups,
                        lvm_logical_volumes,
                        lvm_physical_volumes,
                    )
                }
                Err(e) => {
                    warn!(
                        node_id = %node.id,
                        error = %e,
                        "cannot reach node for storage overview"
                    );
                    (vec![], false, false, vec![], vec![], vec![])
                }
            };

            if disk_inventory_ok {
                nodes_disk_inventory_ok += 1;
                total_block_devices += disks.len() as i32;
            }

            nodes_out.push(controller_proto::NodeStorageOverview {
                node_id: node.id.clone(),
                hostname: node.hostname.clone(),
                address: node.address.clone(),
                storage_backend: storage_backend_to_proto(&node.storage_backend),
                luks_method: node.luks_method.clone(),
                disk_inventory_ok,
                disks,
                lvm_inventory_ok,
                lvm_volume_groups,
                lvm_logical_volumes,
                lvm_physical_volumes,
            });
        }

        Ok(Response::new(
            controller_proto::GetStorageOverviewResponse {
                approved_nodes: approved.len() as i32,
                nodes_disk_inventory_ok,
                backend_filesystem_nodes,
                backend_lvm_nodes,
                backend_zfs_nodes,
                backend_unspecified_nodes,
                backend_ceph_nodes,
                nodes_luks_tpm2,
                nodes_luks_keyfile,
                nodes_luks_unknown,
                total_block_devices,
                nodes: nodes_out,
            },
        ))
    }

    async fn list_volumes(
        &self,
        request: Request<controller_proto::ListVolumesRequest>,
    ) -> Result<Response<controller_proto::ListVolumesResponse>, Status> {
        self.require_operator(&request, OperatorRole::ReadOnly)?;
        let filter_vm = request.into_inner().vm.trim().to_string();

        let vms = self
            .db
            .list_vms()
            .map_err(|e| Status::internal(e.to_string()))?;
        let vm_by_id: HashMap<String, VmRow> = vms.into_iter().map(|v| (v.id.clone(), v)).collect();

        let mut rows = self
            .db
            .list_volumes()
            .map_err(|e| Status::internal(e.to_string()))?;
        if !filter_vm.is_empty() {
            let want_id = vm_by_id
                .values()
                .find(|v| v.id == filter_vm || v.name == filter_vm)
                .map(|v| v.id.clone())
                .unwrap_or_else(|| filter_vm.clone());
            rows.retain(|v| v.vm_id == want_id || v.name == filter_vm || v.id == filter_vm);
        }

        let volumes: Vec<_> = rows
            .into_iter()
            .map(|vol| {
                let vm = vm_by_id.get(&vol.vm_id);
                self.volume_info_from_row(&vol, vm)
            })
            .collect();

        Ok(Response::new(controller_proto::ListVolumesResponse {
            volumes,
        }))
    }

    async fn create_volume(
        &self,
        request: Request<controller_proto::CreateVolumeRequest>,
    ) -> Result<Response<controller_proto::CreateVolumeResponse>, Status> {
        self.require_operator(&request, OperatorRole::VmAdmin)?;
        let actor = Self::audit_actor(&request);
        let req = request.into_inner();
        let name = req.name.trim();
        if name.is_empty() {
            return Err(Status::invalid_argument("volume name is required"));
        }
        let from_snap = req.from_snapshot.trim().to_string();
        if from_snap.is_empty() && req.size_bytes <= 0 {
            return Err(Status::invalid_argument("size_bytes must be positive"));
        }
        if req.encrypt && !from_snap.is_empty() {
            return Err(Status::invalid_argument(
                "encrypt cannot be combined with --from-snapshot in this release",
            ));
        }
        let storage_class = if req.storage_class.trim().is_empty() {
            "ceph"
        } else {
            req.storage_class.trim()
        };
        if crate::host_volume::is_host(storage_class) {
            return self
                .create_host_volume(&actor, name, storage_class, &req)
                .await;
        }
        if storage_class != "ceph" {
            return Err(Status::invalid_argument(
                "storage_class must be ceph, lvm, or zfs",
            ));
        }
        if self
            .db
            .get_volume_by_name(name)
            .map_err(|e| Status::internal(e.to_string()))?
            .is_some()
        {
            return Err(Status::already_exists(format!(
                "volume '{name}' already exists"
            )));
        }

        let parent_snap = if from_snap.is_empty() {
            None
        } else {
            Some(
                self.db
                    .get_volume_snapshot_by_name(&from_snap)
                    .map_err(|e| Status::internal(e.to_string()))?
                    .or(self
                        .db
                        .get_volume_snapshot_by_id(&from_snap)
                        .map_err(|e| Status::internal(e.to_string()))?)
                    .ok_or_else(|| {
                        Status::not_found(format!("snapshot '{from_snap}' not found"))
                    })?,
            )
        };

        let size = if let Some(ref snap) = parent_snap {
            snap.size_bytes
        } else {
            req.size_bytes
        };
        let mut row = VolumeRow::new_data(name, size);
        if !req.guest_format_json.trim().is_empty() {
            row.guest_format_json = req.guest_format_json;
        }
        if let Some(ref snap) = parent_snap {
            row.parent_snapshot_id = snap.id.clone();
            row.source_json = format!(
                r#"{{"snapshotId":"{}","snapshotName":"{}"}}"#,
                snap.id, snap.name
            );
        }

        let node = self
            .pick_healthy_ceph_node()
            .await
            .map_err(Status::failed_precondition)?;
        if self.clients.get_storage(&node.address).is_none() {
            self.clients
                .connect(&node.address)
                .await
                .map_err(|e| Status::unavailable(format!("connecting to Ceph node: {e}")))?;
        }
        let mut storage = self
            .clients
            .get_storage(&node.address)
            .ok_or_else(|| Status::unavailable("Ceph storage client unavailable"))?;
        if let Some(ref snap) = parent_snap {
            let parent_vol = self
                .db
                .get_volume_by_id(&snap.volume_id)
                .map_err(|e| Status::internal(e.to_string()))?
                .ok_or_else(|| Status::failed_precondition("parent volume for snapshot missing"))?;
            storage
                .clone_volume(node_proto::CloneVolumeRequest {
                    parent_handle: format!("{}/{}", parent_vol.pool, parent_vol.image),
                    parent_snapshot: snap.rbd_snap.clone(),
                    child_image: row.image.clone(),
                    storage_class: crate::volume_export::storage_class_for(&parent_vol),
                })
                .await
                .map_err(|e| Status::internal(format!("cloning RBD volume: {e}")))?;
        } else {
            storage
                .create_volume(node_proto::CreateVolumeRequest {
                    volume_id: row.image.clone(),
                    storage_class: "ceph".into(),
                    size_bytes: row.size_bytes,
                    parameters: HashMap::new(),
                    lv_name: String::new(),
                })
                .await
                .map_err(|e| Status::internal(format!("creating RBD volume: {e}")))?;
        }

        if req.encrypt {
            let master = crate::volume_crypto::MasterKeyStore::default()
                .load_or_create()
                .map_err(|e| Status::failed_precondition(format!("volume master key: {e}")))?;
            let dek = crate::volume_crypto::generate_dek()
                .map_err(|e| Status::internal(format!("generate dek: {e}")))?;
            let mapper = crate::volume_crypto::mapper_name_for_serial(&row.serial);
            storage
                .format_encrypted_volume(node_proto::FormatEncryptedVolumeRequest {
                    rbd_device: format!("{}/{}", row.pool, row.image),
                    dek: dek.clone(),
                    mapper_name: mapper.clone(),
                })
                .await
                .map_err(|e| Status::internal(format!("format encrypted volume: {e}")))?;
            // Close mapper after format so the volume can stay detached.
            let _ = storage
                .lock_encrypted_volume(node_proto::LockEncryptedVolumeRequest {
                    mapper_name: mapper,
                })
                .await;
            row.encrypted = true;
            row.wrapped_dek = crate::volume_crypto::wrap_dek(&master, &dek)
                .map_err(|e| Status::internal(format!("wrap dek: {e}")))?;
        }

        let attach_vm = req.vm.trim().to_string();
        if !attach_vm.is_empty() {
            let vm = self
                .resolve_vm_row(&attach_vm)?
                .ok_or_else(|| Status::not_found(format!("VM '{attach_vm}' not found")))?;
            if vm.storage_backend != "ceph" {
                return Err(Status::failed_precondition(
                    "can only attach Ceph volumes to Ceph-backed VMs",
                ));
            }
            let slot = self
                .db
                .next_data_slot_for_vm(&vm.id)
                .map_err(|e| Status::internal(e.to_string()))?;
            row.vm_id = vm.id.clone();
            row.slot = slot;
            row.attach_state = VolumeRow::ATTACH_ATTACHED.into();
            let live = self.vm_is_live(&vm).await?;
            if live {
                self.hotplug_add_volume_disk(&vm, &row).await?;
            }
            self.db
                .upsert_volume(&row)
                .map_err(|e| Status::internal(format!("storing volume: {e}")))?;
            let node = self
                .db
                .get_node(&vm.node_id)
                .map_err(|e| Status::internal(e.to_string()))?
                .ok_or_else(|| Status::failed_precondition("VM node missing"))?;
            self.push_config_and_await_apply(&node).await?;
        } else {
            self.db
                .upsert_volume(&row)
                .map_err(|e| Status::internal(format!("storing volume: {e}")))?;
        }

        self.log_replication_event(
            &actor,
            Some("volume.create"),
            EVT_VOLUME_CREATE,
            &format!("volume/{}", row.name),
            serde_json::json!({
                "id": row.id,
                "name": row.name,
                "role": row.role,
                "pool": row.pool,
                "image": row.image,
                "sizeBytes": row.size_bytes,
                "vmId": row.vm_id,
                "attachState": row.attach_state,
                "encrypted": row.encrypted,
                "parentSnapshotId": row.parent_snapshot_id,
                "serial": row.serial,
                "slot": row.slot,
                "storageClass": row.storage_class,
                "sourceJson": row.source_json,
            }),
        );

        let vm = if row.vm_id.is_empty() {
            None
        } else {
            self.db
                .get_vm(&row.vm_id)
                .map_err(|e| Status::internal(e.to_string()))?
        };
        Ok(Response::new(controller_proto::CreateVolumeResponse {
            success: true,
            volume: Some(self.volume_info_from_row(&row, vm.as_ref())),
            message: format!("volume '{name}' created"),
        }))
    }

    async fn get_volume(
        &self,
        request: Request<controller_proto::GetVolumeRequest>,
    ) -> Result<Response<controller_proto::GetVolumeResponse>, Status> {
        self.require_operator(&request, OperatorRole::ReadOnly)?;
        let name = request.into_inner().name.trim().to_string();
        let row = self
            .resolve_volume_row(&name)?
            .ok_or_else(|| Status::not_found(format!("volume '{name}' not found")))?;
        let vm = if row.vm_id.is_empty() {
            None
        } else {
            self.db
                .get_vm(&row.vm_id)
                .map_err(|e| Status::internal(e.to_string()))?
        };
        Ok(Response::new(controller_proto::GetVolumeResponse {
            volume: Some(self.volume_info_from_row(&row, vm.as_ref())),
        }))
    }

    async fn attach_volume(
        &self,
        request: Request<controller_proto::AttachVolumeRequest>,
    ) -> Result<Response<controller_proto::AttachVolumeResponse>, Status> {
        self.require_operator(&request, OperatorRole::VmAdmin)?;
        let actor = Self::audit_actor(&request);
        let req = request.into_inner();
        let mut row = self
            .resolve_volume_row(req.name.trim())?
            .ok_or_else(|| Status::not_found(format!("volume '{}' not found", req.name)))?;
        if row.role == VolumeRow::ROLE_ROOT {
            return Err(Status::failed_precondition(
                "root volumes cannot be re-attached",
            ));
        }
        if crate::host_volume::is_host(&row.storage_class) {
            return Err(Status::failed_precondition(
                "lvm and zfs volumes stay on their node; mount them instead of attaching to a VM",
            ));
        }
        if row.attach_state == VolumeRow::ATTACH_ATTACHED && !row.vm_id.is_empty() {
            return Err(Status::failed_precondition(format!(
                "volume '{}' is already attached to {}",
                row.name, row.vm_id
            )));
        }
        let vm = self
            .resolve_vm_row(req.vm.trim())?
            .ok_or_else(|| Status::not_found(format!("VM '{}' not found", req.vm)))?;
        if vm.storage_backend != "ceph" {
            return Err(Status::failed_precondition(
                "can only attach Ceph volumes to Ceph-backed VMs",
            ));
        }
        let live = self.vm_is_live(&vm).await?;
        let slot = self
            .db
            .next_data_slot_for_vm(&vm.id)
            .map_err(|e| Status::internal(e.to_string()))?;
        row.vm_id = vm.id.clone();
        row.slot = slot;
        row.attach_state = VolumeRow::ATTACH_ATTACHED.into();
        row.generation += 1;
        if live {
            self.hotplug_add_volume_disk(&vm, &row).await?;
        }
        self.db
            .upsert_volume(&row)
            .map_err(|e| Status::internal(e.to_string()))?;
        let node = self
            .db
            .get_node(&vm.node_id)
            .map_err(|e| Status::internal(e.to_string()))?
            .ok_or_else(|| Status::failed_precondition("VM node missing"))?;
        // Persist disk in Nix even after hotplug so reboot keeps the attach.
        self.push_config_and_await_apply(&node).await?;
        self.log_replication_event(
            &actor,
            Some("volume.attach"),
            EVT_VOLUME_ATTACH,
            &format!("volume/{}", row.name),
            serde_json::json!({
                "id": row.id,
                "name": row.name,
                "vmId": row.vm_id,
                "slot": row.slot,
                "attachState": row.attach_state,
                "generation": row.generation,
            }),
        );
        let how = if live { "hot-attached" } else { "attached" };
        Ok(Response::new(controller_proto::AttachVolumeResponse {
            success: true,
            message: format!("{how} '{}' to '{}'", row.name, vm.name),
            volume: Some(self.volume_info_from_row(&row, Some(&vm))),
        }))
    }

    async fn detach_volume(
        &self,
        request: Request<controller_proto::DetachVolumeRequest>,
    ) -> Result<Response<controller_proto::DetachVolumeResponse>, Status> {
        self.require_operator(&request, OperatorRole::VmAdmin)?;
        let actor = Self::audit_actor(&request);
        let name = request.into_inner().name;
        let mut row = self
            .resolve_volume_row(name.trim())?
            .ok_or_else(|| Status::not_found(format!("volume '{name}' not found")))?;
        if row.role == VolumeRow::ROLE_ROOT {
            return Err(Status::failed_precondition(
                "root volumes cannot be detached; delete the VM instead",
            ));
        }
        if row.vm_id.is_empty() || row.attach_state == VolumeRow::ATTACH_DETACHED {
            return Ok(Response::new(controller_proto::DetachVolumeResponse {
                success: true,
                message: format!("volume '{}' already detached", row.name),
                volume: Some(self.volume_info_from_row(&row, None)),
            }));
        }
        let vm = self
            .db
            .get_vm(&row.vm_id)
            .map_err(|e| Status::internal(e.to_string()))?
            .ok_or_else(|| Status::failed_precondition("attached VM missing"))?;
        let live = self.vm_is_live(&vm).await?;
        let node = self
            .db
            .get_node(&vm.node_id)
            .map_err(|e| Status::internal(e.to_string()))?
            .ok_or_else(|| Status::failed_precondition("VM node missing"))?;
        if live {
            self.hotplug_remove_volume_disk(&vm, &row).await?;
        }
        row.vm_id = String::new();
        row.slot = 0;
        row.attach_state = VolumeRow::ATTACH_DETACHED.into();
        row.generation += 1;
        self.db
            .upsert_volume(&row)
            .map_err(|e| Status::internal(e.to_string()))?;
        self.push_config_and_await_apply(&node).await?;
        self.lock_encrypted_volumes_on_node(&node, std::slice::from_ref(&row))
            .await;
        self.log_replication_event(
            &actor,
            Some("volume.detach"),
            EVT_VOLUME_DETACH,
            &format!("volume/{}", row.name),
            serde_json::json!({
                "id": row.id,
                "name": row.name,
                "attachState": row.attach_state,
                "generation": row.generation,
            }),
        );
        let how = if live { "hot-detached" } else { "detached" };
        Ok(Response::new(controller_proto::DetachVolumeResponse {
            success: true,
            message: format!("{how} '{}'", row.name),
            volume: Some(self.volume_info_from_row(&row, None)),
        }))
    }

    async fn delete_volume(
        &self,
        request: Request<controller_proto::DeleteVolumeRequest>,
    ) -> Result<Response<controller_proto::DeleteVolumeResponse>, Status> {
        self.require_operator(&request, OperatorRole::BackupOperator)?;
        let role = crate::auth::controller_operator_role(
            &request,
            &self.db,
            self.tls_paths.is_some(),
            self.bootstrap_kctl,
        )?;
        let actor = Self::audit_actor(&request);
        let name = request.into_inner().name;
        let row = self
            .resolve_volume_row(name.trim())?
            .ok_or_else(|| Status::not_found(format!("volume '{name}' not found")))?;
        let labelled = if row.vm_id.is_empty() {
            false
        } else {
            self.db
                .get_vm(&row.vm_id)
                .ok()
                .flatten()
                .is_some_and(|vm| restore_labelled(&vm.labels))
        };
        if !role.satisfies(OperatorRole::VmAdmin) && !labelled {
            return Err(Status::permission_denied(
                "backup-operator can delete only volumes of a VM labelled kcore-vault/restore",
            ));
        }
        if row.role == VolumeRow::ROLE_ROOT {
            return Err(Status::failed_precondition(
                "root volumes are deleted with the VM",
            ));
        }
        if row.attach_state != VolumeRow::ATTACH_DETACHED || !row.vm_id.is_empty() {
            return Err(Status::failed_precondition(
                "detach the volume before deleting it",
            ));
        }
        let snapshots = self
            .db
            .list_volume_snapshots(Some(&row.id))
            .map_err(|e| Status::internal(e.to_string()))?;
        if !snapshots.is_empty() {
            return Err(Status::failed_precondition(format!(
                "volume '{}' still has {} snapshot(s); delete them first (kctl delete volume-snapshot)",
                row.name,
                snapshots.len()
            )));
        }
        if crate::host_volume::is_host(&row.storage_class) {
            return self.delete_host_volume(actor, row).await;
        }
        let node = self
            .pick_healthy_ceph_node()
            .await
            .map_err(Status::failed_precondition)?;
        if self.clients.get_storage(&node.address).is_none() {
            let _ = self.clients.connect(&node.address).await;
        }
        if let Some(mut storage) = self.clients.get_storage(&node.address) {
            let handle = format!("{}/{}", row.pool, row.image);
            storage
                .delete_volume(node_proto::DeleteVolumeRequest {
                    backend_handle: handle,
                })
                .await
                .map_err(|e| Status::internal(format!("deleting RBD image: {e}")))?;
        }
        self.db
            .delete_volume_by_id(&row.id)
            .map_err(|e| Status::internal(e.to_string()))?;
        self.log_replication_event(
            &actor,
            Some("volume.delete"),
            EVT_VOLUME_DELETE,
            &format!("volume/{}", row.name),
            serde_json::json!({
                "id": row.id,
                "name": row.name,
            }),
        );
        Ok(Response::new(controller_proto::DeleteVolumeResponse {
            success: true,
            message: format!("deleted volume '{}'", row.name),
        }))
    }

    async fn list_vm_operations(
        &self,
        request: Request<controller_proto::ListVmOperationsRequest>,
    ) -> Result<Response<controller_proto::ListVmOperationsResponse>, Status> {
        self.require_operator(&request, OperatorRole::ReadOnly)?;
        let req = request.into_inner();
        let mut ops = self
            .db
            .list_vm_operations(req.include_finished)
            .map_err(|e| Status::internal(e.to_string()))?;
        if !req.vm_id.trim().is_empty() {
            let key = req.vm_id.trim();
            let vm_id = self
                .resolve_vm_row(key)?
                .map(|v| v.id)
                .unwrap_or_else(|| key.to_string());
            ops.retain(|o| o.vm_id == vm_id);
        }
        Ok(Response::new(controller_proto::ListVmOperationsResponse {
            operations: ops.iter().map(Self::vm_operation_to_proto).collect(),
        }))
    }

    async fn get_vm_operation(
        &self,
        request: Request<controller_proto::GetVmOperationRequest>,
    ) -> Result<Response<controller_proto::GetVmOperationResponse>, Status> {
        self.require_operator(&request, OperatorRole::ReadOnly)?;
        let id = request.into_inner().id;
        let mut op = self
            .db
            .get_vm_operation(id.trim())
            .map_err(|e| Status::internal(e.to_string()))?
            .ok_or_else(|| Status::not_found(format!("operation '{id}' not found")))?;
        if op.kind == VmOperationRow::KIND_LIVE_MIGRATE && op.finished_at.is_empty() {
            if let Ok(progress_json) = self.live_migrate_progress_json(&op).await {
                op.detail_json = merge_detail_json(&op.detail_json, &progress_json);
            }
        }
        Ok(Response::new(controller_proto::GetVmOperationResponse {
            operation: Some(Self::vm_operation_to_proto(&op)),
        }))
    }

    async fn cancel_vm_operation(
        &self,
        request: Request<controller_proto::CancelVmOperationRequest>,
    ) -> Result<Response<controller_proto::CancelVmOperationResponse>, Status> {
        self.require_operator(&request, OperatorRole::VmAdmin)?;
        let id = request.into_inner().id;
        let op = self
            .db
            .get_vm_operation(id.trim())
            .map_err(|e| Status::internal(e.to_string()))?
            .ok_or_else(|| Status::not_found(format!("operation '{id}' not found")))?;
        if !op.finished_at.is_empty() {
            return Err(Status::failed_precondition(format!(
                "operation '{id}' already finished in phase {}",
                op.phase
            )));
        }
        if !crate::vm_operation_policy::cancel_allowed(&op.phase, op.send_succeeded) {
            return Err(Status::failed_precondition(format!(
                "cancel refused in phase {} (send_succeeded={}; only Preparing, MirrorSyncing, or Copying before send is cancellable; after send the guest may already be on the destination)",
                op.phase, op.send_succeeded
            )));
        }
        self.db
            .request_cancel_vm_operation(&op.id)
            .map_err(|e| Status::internal(e.to_string()))?;
        let updated = self
            .db
            .get_vm_operation(&op.id)
            .map_err(|e| Status::internal(e.to_string()))?
            .unwrap_or(op);
        Ok(Response::new(controller_proto::CancelVmOperationResponse {
            success: true,
            message: format!(
                "cancel requested for operation {}; migrate will abort if still Preparing",
                updated.id
            ),
            operation: Some(Self::vm_operation_to_proto(&updated)),
        }))
    }

    async fn create_volume_snapshot(
        &self,
        request: Request<controller_proto::CreateVolumeSnapshotRequest>,
    ) -> Result<Response<controller_proto::CreateVolumeSnapshotResponse>, Status> {
        self.require_operator(&request, OperatorRole::BackupOperator)?;
        let actor = Self::audit_actor(&request);
        let req = request.into_inner();
        let vol = self
            .resolve_volume_row(req.volume.trim())?
            .ok_or_else(|| Status::not_found(format!("volume '{}' not found", req.volume)))?;
        crate::host_volume::require_snapshotable(&vol.storage_class, "snapshot")
            .map_err(Status::failed_precondition)?;
        if !vol.vm_id.is_empty() {
            if let Ok(Some(open)) = self.db.get_open_vm_operation(&vol.vm_id) {
                return Err(Status::failed_precondition(format!(
                    "refusing snapshot while VM has open operation {} ({})",
                    open.id, open.phase
                )));
            }
        }
        let snap_name = if req.name.trim().is_empty() {
            let secs = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0);
            format!("{}-{secs}", vol.name)
        } else {
            req.name.trim().to_string()
        };
        if self
            .db
            .get_volume_snapshot_by_name(&snap_name)
            .map_err(|e| Status::internal(e.to_string()))?
            .is_some()
        {
            return Err(Status::already_exists(format!(
                "snapshot '{snap_name}' already exists"
            )));
        }
        let consistency = volume_snapshot::normalize_snapshot_consistency(&req.consistency)
            .map_err(Status::invalid_argument)?;
        let vm_for_quiesce = if vol.vm_id.is_empty() {
            None
        } else {
            self.db
                .get_vm(&vol.vm_id)
                .map_err(|e| Status::internal(e.to_string()))?
        };
        let vm_live = if let Some(ref vm_row) = vm_for_quiesce {
            self.vm_is_live(vm_row).await?
        } else {
            false
        };
        let needs_quiesce =
            volume_snapshot::snapshot_needs_guest_quiesce(&consistency, &vol, vm_live);
        let mut guest_frozen = false;
        if needs_quiesce {
            let vm_row = vm_for_quiesce.as_ref().expect("attached volume has vm");
            self.guest_fs_freeze_for_vm(vm_row, true, "", "").await?;
            guest_frozen = true;
        }
        let rbd_snap = VolumeRow::serial_from_id(&Uuid::new_v4().to_string());
        let node = self
            .resolve_snapshot_node(&vol)
            .await
            .map_err(Status::failed_precondition)?;
        if self.clients.get_storage(&node.address).is_none() {
            let _ = self.clients.connect(&node.address).await;
        }
        let mut storage = self
            .clients
            .get_storage(&node.address)
            .ok_or_else(|| Status::unavailable("storage client unavailable"))?;
        let storage_class = crate::volume_export::storage_class_for(&vol);
        let snap_result = storage
            .snapshot_volume(node_proto::SnapshotVolumeRequest {
                backend_handle: crate::volume_export::backend_handle_for(&vol),
                snapshot_name: rbd_snap.clone(),
                protect: true,
                storage_class,
            })
            .await
            .map_err(|e| Status::internal(format!("snap create: {e}")));
        if guest_frozen {
            if let Some(ref vm_row) = vm_for_quiesce {
                if let Err(unfreeze_err) = self.guest_fs_freeze_for_vm(vm_row, false, "", "").await
                {
                    warn!(
                        vm = %vm_row.name,
                        error = %unfreeze_err,
                        "guest fs unfreeze after snapshot failed"
                    );
                }
            }
        }
        snap_result?;
        let row = VolumeSnapshotRow {
            id: Uuid::new_v4().to_string(),
            name: snap_name.clone(),
            volume_id: vol.id.clone(),
            rbd_snap,
            protected: true,
            size_bytes: vol.size_bytes,
            consistency,
            created_at: String::new(),
        };
        self.db
            .insert_volume_snapshot(&row)
            .map_err(|e| Status::internal(e.to_string()))?;
        self.log_replication_event(
            &actor,
            Some("volume_snapshot.create"),
            EVT_VOLUME_SNAPSHOT_CREATE,
            &format!("volume_snapshot/{}", row.name),
            serde_json::json!({
                "id": row.id,
                "name": row.name,
                "volumeId": row.volume_id,
                "rbdSnap": row.rbd_snap,
                "protected": row.protected,
                "sizeBytes": row.size_bytes,
                "consistency": row.consistency,
            }),
        );
        Ok(Response::new(
            controller_proto::CreateVolumeSnapshotResponse {
                success: true,
                message: format!("snapshot '{snap_name}' created"),
                snapshot: Some(self.volume_snapshot_to_proto(&row, Some(&vol))),
            },
        ))
    }

    async fn list_volume_snapshots(
        &self,
        request: Request<controller_proto::ListVolumeSnapshotsRequest>,
    ) -> Result<Response<controller_proto::ListVolumeSnapshotsResponse>, Status> {
        self.require_operator(&request, OperatorRole::ReadOnly)?;
        let filter = request.into_inner().volume.trim().to_string();
        let volume_id = if filter.is_empty() {
            None
        } else {
            Some(
                self.resolve_volume_row(&filter)?
                    .map(|v| v.id)
                    .unwrap_or(filter),
            )
        };
        let snaps = self
            .db
            .list_volume_snapshots(volume_id.as_deref())
            .map_err(|e| Status::internal(e.to_string()))?;
        let mut out = Vec::with_capacity(snaps.len());
        for s in snaps {
            let vol = self
                .db
                .get_volume_by_id(&s.volume_id)
                .map_err(|e| Status::internal(e.to_string()))?;
            out.push(self.volume_snapshot_to_proto(&s, vol.as_ref()));
        }
        Ok(Response::new(
            controller_proto::ListVolumeSnapshotsResponse { snapshots: out },
        ))
    }

    async fn delete_volume_snapshot(
        &self,
        request: Request<controller_proto::DeleteVolumeSnapshotRequest>,
    ) -> Result<Response<controller_proto::DeleteVolumeSnapshotResponse>, Status> {
        self.require_operator(&request, OperatorRole::BackupOperator)?;
        let actor = Self::audit_actor(&request);
        let key = request.into_inner().name.trim().to_string();
        let snap = self
            .db
            .get_volume_snapshot_by_name(&key)
            .map_err(|e| Status::internal(e.to_string()))?
            .or(self
                .db
                .get_volume_snapshot_by_id(&key)
                .map_err(|e| Status::internal(e.to_string()))?)
            .ok_or_else(|| Status::not_found(format!("snapshot '{key}' not found")))?;
        let children = self
            .db
            .count_volumes_with_parent_snapshot(&snap.id)
            .map_err(|e| Status::internal(e.to_string()))?;
        if children > 0 {
            return Err(Status::failed_precondition(format!(
                "snapshot '{}' still has {children} unflattened clone(s); flatten or delete them first",
                snap.name
            )));
        }
        let vol = self
            .db
            .get_volume_by_id(&snap.volume_id)
            .map_err(|e| Status::internal(e.to_string()))?
            .ok_or_else(|| Status::failed_precondition("parent volume missing"))?;
        let node = self
            .resolve_snapshot_node(&vol)
            .await
            .map_err(Status::failed_precondition)?;
        if self.clients.get_storage(&node.address).is_none() {
            let _ = self.clients.connect(&node.address).await;
        }
        if let Some(mut storage) = self.clients.get_storage(&node.address) {
            storage
                .delete_volume_snapshot(node_proto::DeleteVolumeSnapshotRequest {
                    backend_handle: crate::volume_export::backend_handle_for(&vol),
                    snapshot_name: snap.rbd_snap.clone(),
                    unprotect: snap.protected,
                    storage_class: crate::volume_export::storage_class_for(&vol),
                })
                .await
                .map_err(|e| Status::internal(format!("snap rm: {e}")))?;
        }
        self.db
            .delete_volume_snapshot(&snap.id)
            .map_err(|e| Status::internal(e.to_string()))?;
        self.log_replication_event(
            &actor,
            Some("volume_snapshot.delete"),
            EVT_VOLUME_SNAPSHOT_DELETE,
            &format!("volume_snapshot/{}", snap.name),
            serde_json::json!({
                "id": snap.id,
                "name": snap.name,
                "volumeId": snap.volume_id,
            }),
        );
        Ok(Response::new(
            controller_proto::DeleteVolumeSnapshotResponse {
                success: true,
                message: format!("deleted snapshot '{}'", snap.name),
            },
        ))
    }

    async fn restore_volume(
        &self,
        request: Request<controller_proto::RestoreVolumeRequest>,
    ) -> Result<Response<controller_proto::RestoreVolumeResponse>, Status> {
        self.require_operator(&request, OperatorRole::VmAdmin)?;
        let req = request.into_inner();
        let mut vol = self
            .resolve_volume_row(req.volume.trim())?
            .ok_or_else(|| Status::not_found(format!("volume '{}' not found", req.volume)))?;
        crate::host_volume::require_snapshotable(&vol.storage_class, "restore")
            .map_err(Status::failed_precondition)?;
        let snap = self
            .db
            .get_volume_snapshot_by_name(req.snapshot.trim())
            .map_err(|e| Status::internal(e.to_string()))?
            .or(self
                .db
                .get_volume_snapshot_by_id(req.snapshot.trim())
                .map_err(|e| Status::internal(e.to_string()))?)
            .ok_or_else(|| Status::not_found(format!("snapshot '{}' not found", req.snapshot)))?;
        if snap.volume_id != vol.id {
            return Err(Status::invalid_argument(
                "snapshot does not belong to this volume",
            ));
        }
        if !vol.vm_id.is_empty() {
            let vm = self
                .db
                .get_vm(&vol.vm_id)
                .map_err(|e| Status::internal(e.to_string()))?
                .ok_or_else(|| Status::failed_precondition("attached VM missing"))?;
            self.require_vm_stopped_for_volume_change(&vm).await?;
            let node = self
                .db
                .get_node(&vm.node_id)
                .map_err(|e| Status::internal(e.to_string()))?
                .ok_or_else(|| Status::failed_precondition("VM node missing"))?;
            // Unmap via cold release so rbd rollback has no watchers.
            if crate::host_volume::normalized_storage_class(&vol.storage_class) == "ceph" {
                self.cold_release_ceph_vm(&vm, &node).await?;
            }
        } else if vol.attach_state != VolumeRow::ATTACH_DETACHED {
            return Err(Status::failed_precondition(
                "volume must be detached (or its VM stopped) before in-place restore",
            ));
        }
        let node = self
            .resolve_snapshot_node(&vol)
            .await
            .map_err(Status::failed_precondition)?;
        if self.clients.get_storage(&node.address).is_none() {
            let _ = self.clients.connect(&node.address).await;
        }
        let mut storage = self
            .clients
            .get_storage(&node.address)
            .ok_or_else(|| Status::unavailable("storage client unavailable"))?;
        storage
            .rollback_volume(node_proto::RollbackVolumeRequest {
                backend_handle: crate::volume_export::backend_handle_for(&vol),
                snapshot_name: snap.rbd_snap.clone(),
                storage_class: crate::volume_export::storage_class_for(&vol),
            })
            .await
            .map_err(|e| Status::internal(format!("snap rollback: {e}")))?;
        vol.generation += 1;
        self.db
            .upsert_volume(&vol)
            .map_err(|e| Status::internal(e.to_string()))?;
        Ok(Response::new(controller_proto::RestoreVolumeResponse {
            success: true,
            message: format!(
                "restored volume '{}' from snapshot '{}'",
                vol.name, snap.name
            ),
        }))
    }

    async fn flatten_volume(
        &self,
        request: Request<controller_proto::FlattenVolumeRequest>,
    ) -> Result<Response<controller_proto::FlattenVolumeResponse>, Status> {
        self.require_operator(&request, OperatorRole::VmAdmin)?;
        let name = request.into_inner().name;
        let mut vol = self
            .resolve_volume_row(name.trim())?
            .ok_or_else(|| Status::not_found(format!("volume '{name}' not found")))?;
        crate::host_volume::require_ceph(&vol.storage_class, "flatten")
            .map_err(Status::failed_precondition)?;
        if vol.parent_snapshot_id.is_empty() {
            return Err(Status::failed_precondition(
                "volume is not a clone (no parent_snapshot_id)",
            ));
        }
        if !vol.vm_id.is_empty() {
            let vm = self
                .db
                .get_vm(&vol.vm_id)
                .map_err(|e| Status::internal(e.to_string()))?
                .ok_or_else(|| Status::failed_precondition("attached VM missing"))?;
            self.require_vm_stopped_for_volume_change(&vm).await?;
        }
        let node = self
            .pick_healthy_ceph_node()
            .await
            .map_err(Status::failed_precondition)?;
        if self.clients.get_storage(&node.address).is_none() {
            let _ = self.clients.connect(&node.address).await;
        }
        let mut storage = self
            .clients
            .get_storage(&node.address)
            .ok_or_else(|| Status::unavailable("Ceph storage client unavailable"))?;
        storage
            .flatten_volume(node_proto::FlattenVolumeRequest {
                backend_handle: format!("{}/{}", vol.pool, vol.image),
            })
            .await
            .map_err(|e| Status::internal(format!("rbd flatten: {e}")))?;
        vol.parent_snapshot_id.clear();
        vol.source_json = "{}".into();
        vol.generation += 1;
        self.db
            .upsert_volume(&vol)
            .map_err(|e| Status::internal(e.to_string()))?;
        Ok(Response::new(controller_proto::FlattenVolumeResponse {
            success: true,
            message: format!("flattened volume '{}'", vol.name),
        }))
    }

    async fn expose_volume_snapshot(
        &self,
        request: Request<controller_proto::ExposeVolumeSnapshotRequest>,
    ) -> Result<Response<controller_proto::ExposeVolumeSnapshotResponse>, Status> {
        self.require_operator(&request, OperatorRole::BackupOperator)?;
        let req = request.into_inner();
        let vol = self
            .resolve_volume_row(req.volume_name.trim())?
            .ok_or_else(|| Status::not_found(format!("volume '{}' not found", req.volume_name)))?;
        crate::host_volume::require_snapshotable(&vol.storage_class, "expose")
            .map_err(Status::failed_precondition)?;
        let snap = self
            .db
            .get_volume_snapshot_by_name(req.snapshot_name.trim())
            .map_err(|e| Status::internal(e.to_string()))?
            .ok_or_else(|| {
                Status::not_found(format!("snapshot '{}' not found", req.snapshot_name))
            })?;
        if snap.volume_id != vol.id {
            return Err(Status::invalid_argument(
                "snapshot does not belong to this volume",
            ));
        }
        let node = self
            .resolve_snapshot_node(&vol)
            .await
            .map_err(Status::failed_precondition)?;
        if self.clients.get_storage(&node.address).is_none() {
            let _ = self.clients.connect(&node.address).await;
        }
        let mut storage = self
            .clients
            .get_storage(&node.address)
            .ok_or_else(|| Status::unavailable("storage client unavailable"))?;
        let exposed = storage
            .expose_snapshot(node_proto::ExposeSnapshotRequest {
                backend_handle: crate::volume_export::backend_handle_for(&vol),
                snapshot_name: snap.rbd_snap.clone(),
                storage_class: crate::volume_export::storage_class_for(&vol),
                lease_seconds: req.lease_seconds,
            })
            .await
            .map_err(|e| Status::internal(format!("expose snapshot: {e}")))?
            .into_inner();
        crate::volume_export::global_export_leases().insert(crate::volume_export::ExportLease {
            lease_id: exposed.lease_id.clone(),
            node_id: node.id.clone(),
            node_address: node.address.clone(),
            volume_name: vol.name.clone(),
            snapshot_name: snap.name.clone(),
            device_path: exposed.device_path.clone(),
            size_bytes: exposed.size_bytes,
            block_size: exposed.block_size,
            expires_at: std::time::Instant::now()
                + crate::volume_export::lease_ttl(req.lease_seconds),
        });
        Ok(Response::new(
            controller_proto::ExposeVolumeSnapshotResponse {
                lease_id: exposed.lease_id,
                node_id: node.id,
                node_address: node.address,
                device_path: exposed.device_path,
                size_bytes: exposed.size_bytes,
                block_size: exposed.block_size,
            },
        ))
    }

    async fn unexpose_volume_snapshot(
        &self,
        request: Request<controller_proto::UnexposeVolumeSnapshotRequest>,
    ) -> Result<Response<controller_proto::UnexposeVolumeSnapshotResponse>, Status> {
        self.require_operator(&request, OperatorRole::BackupOperator)?;
        let lease_id = request.into_inner().lease_id;
        let lease = crate::volume_export::global_export_leases()
            .remove(&lease_id)
            .ok_or_else(|| Status::not_found(format!("lease '{lease_id}' not found")))?;
        if self.clients.get_storage(&lease.node_address).is_none() {
            let _ = self.clients.connect(&lease.node_address).await;
        }
        if let Some(mut storage) = self.clients.get_storage(&lease.node_address) {
            storage
                .unexpose_snapshot(node_proto::UnexposeSnapshotRequest {
                    lease_id: lease.lease_id,
                })
                .await
                .map_err(|e| Status::internal(format!("unexpose snapshot: {e}")))?;
        }
        Ok(Response::new(
            controller_proto::UnexposeVolumeSnapshotResponse {},
        ))
    }

    async fn list_volume_changed_extents(
        &self,
        request: Request<controller_proto::ListVolumeChangedExtentsRequest>,
    ) -> Result<Response<controller_proto::ListVolumeChangedExtentsResponse>, Status> {
        self.require_operator(&request, OperatorRole::BackupOperator)?;
        let req = request.into_inner();
        let vol = self
            .resolve_volume_row(req.volume_name.trim())?
            .ok_or_else(|| Status::not_found(format!("volume '{}' not found", req.volume_name)))?;
        crate::host_volume::require_snapshotable(&vol.storage_class, "list changed extents")
            .map_err(Status::failed_precondition)?;
        let to = self
            .db
            .get_volume_snapshot_by_name(req.to_snapshot.trim())
            .map_err(|e| Status::internal(e.to_string()))?
            .ok_or_else(|| {
                Status::not_found(format!("snapshot '{}' not found", req.to_snapshot))
            })?;
        if to.volume_id != vol.id {
            return Err(Status::invalid_argument(
                "to_snapshot does not belong to this volume",
            ));
        }
        let from_backend = if req.from_snapshot.trim().is_empty() {
            String::new()
        } else {
            let from = self
                .db
                .get_volume_snapshot_by_name(req.from_snapshot.trim())
                .map_err(|e| Status::internal(e.to_string()))?
                .ok_or_else(|| {
                    Status::not_found(format!("snapshot '{}' not found", req.from_snapshot))
                })?;
            if from.volume_id != vol.id {
                return Err(Status::invalid_argument(
                    "from_snapshot does not belong to this volume",
                ));
            }
            from.rbd_snap
        };
        let node = self
            .resolve_snapshot_node(&vol)
            .await
            .map_err(Status::failed_precondition)?;
        if self.clients.get_storage(&node.address).is_none() {
            let _ = self.clients.connect(&node.address).await;
        }
        let mut storage = self
            .clients
            .get_storage(&node.address)
            .ok_or_else(|| Status::unavailable("storage client unavailable"))?;
        let resp = storage
            .list_changed_extents(node_proto::ListChangedExtentsRequest {
                backend_handle: crate::volume_export::backend_handle_for(&vol),
                storage_class: crate::volume_export::storage_class_for(&vol),
                from_snapshot: from_backend,
                to_snapshot: to.rbd_snap,
            })
            .await
            .map_err(|e| Status::internal(format!("list changed extents: {e}")))?
            .into_inner();
        Ok(Response::new(
            controller_proto::ListVolumeChangedExtentsResponse {
                extents: resp
                    .extents
                    .into_iter()
                    .map(|e| controller_proto::VolumeExtent {
                        offset: e.offset,
                        length: e.length,
                    })
                    .collect(),
                supported: resp.supported,
                source: resp.source,
            },
        ))
    }

    type ReadVolumeBlocksStream =
        Pin<Box<dyn Stream<Item = Result<controller_proto::VolumeBlock, Status>> + Send>>;

    async fn read_volume_blocks(
        &self,
        request: Request<controller_proto::ReadVolumeBlocksRequest>,
    ) -> Result<Response<Self::ReadVolumeBlocksStream>, Status> {
        self.require_operator(&request, OperatorRole::BackupOperator)?;
        let req = request.into_inner();
        let lease = crate::volume_export::global_export_leases()
            .get(&req.lease_id)
            .ok_or_else(|| Status::not_found(format!("lease '{}' not found", req.lease_id)))?;
        if self.clients.get_storage(&lease.node_address).is_none() {
            let _ = self.clients.connect(&lease.node_address).await;
        }
        let mut storage = self
            .clients
            .get_storage(&lease.node_address)
            .ok_or_else(|| Status::unavailable("storage client unavailable"))?;
        let node_stream = storage
            .read_volume_extents(node_proto::ReadVolumeExtentsRequest {
                lease_id: req.lease_id,
                extents: req
                    .extents
                    .into_iter()
                    .map(|e| node_proto::Extent {
                        offset: e.offset,
                        length: e.length,
                    })
                    .collect(),
                block_size: req.block_size,
                elide_zero_blocks: req.elide_zero_blocks,
            })
            .await
            .map_err(|e| Status::internal(format!("read volume blocks: {e}")))?
            .into_inner();
        let (tx, rx) = mpsc::channel(8);
        tokio::spawn(async move {
            let mut stream = node_stream;
            while let Ok(Some(block)) = stream.message().await {
                let mapped = controller_proto::VolumeBlock {
                    offset: block.offset,
                    body: match block.body {
                        Some(node_proto::volume_block::Body::Data(d)) => {
                            Some(controller_proto::volume_block::Body::Data(d))
                        }
                        Some(node_proto::volume_block::Body::ZeroLength(n)) => {
                            Some(controller_proto::volume_block::Body::ZeroLength(n))
                        }
                        None => None,
                    },
                };
                if tx.send(Ok(mapped)).await.is_err() {
                    break;
                }
            }
        });
        let out: Self::ReadVolumeBlocksStream = Box::pin(ReceiverStream::new(rx));
        Ok(Response::new(out))
    }

    async fn write_volume_blocks(
        &self,
        request: Request<tonic::Streaming<controller_proto::WriteVolumeBlocksRequest>>,
    ) -> Result<Response<controller_proto::WriteVolumeBlocksResponse>, Status> {
        self.require_operator(&request, OperatorRole::BackupOperator)?;
        let mut inbound = request.into_inner();
        let first = inbound
            .message()
            .await?
            .ok_or_else(|| Status::invalid_argument("write stream is empty"))?;
        let target = match first.msg {
            Some(controller_proto::write_volume_blocks_request::Msg::Target(t)) => t,
            _ => {
                return Err(Status::invalid_argument(
                    "first write message must set target",
                ));
            }
        };
        let vol = self
            .resolve_volume_row(target.volume_name.trim())?
            .ok_or_else(|| {
                Status::not_found(format!("volume '{}' not found", target.volume_name))
            })?;
        if vol.is_attached() {
            return Err(Status::failed_precondition(
                "volume must be detached before WriteVolumeBlocks",
            ));
        }
        let node = self
            .resolve_snapshot_node(&vol)
            .await
            .map_err(Status::failed_precondition)?;
        if self.clients.get_storage(&node.address).is_none() {
            let _ = self.clients.connect(&node.address).await;
        }
        let mut storage = self
            .clients
            .get_storage(&node.address)
            .ok_or_else(|| Status::unavailable("storage client unavailable"))?;
        let (tx, rx) = mpsc::channel(8);
        let backend_handle = crate::volume_export::backend_handle_for(&vol);
        let storage_class = crate::volume_export::storage_class_for(&vol);
        let punch = target.punch_zero_runs;
        tx.send(node_proto::WriteVolumeExtentsRequest {
            msg: Some(node_proto::write_volume_extents_request::Msg::Target(
                node_proto::WriteVolumeTarget {
                    backend_handle,
                    storage_class,
                    punch_zero_runs: punch,
                },
            )),
        })
        .await
        .map_err(|_| Status::internal("failed to queue write target"))?;
        tokio::spawn(async move {
            while let Ok(Some(msg)) = inbound.message().await {
                let block = match msg.msg {
                    Some(controller_proto::write_volume_blocks_request::Msg::Block(b)) => b,
                    _ => continue,
                };
                let mapped = node_proto::WriteVolumeExtentsRequest {
                    msg: Some(node_proto::write_volume_extents_request::Msg::Block(
                        node_proto::VolumeBlock {
                            offset: block.offset,
                            body: match block.body {
                                Some(controller_proto::volume_block::Body::Data(d)) => {
                                    Some(node_proto::volume_block::Body::Data(d))
                                }
                                Some(controller_proto::volume_block::Body::ZeroLength(n)) => {
                                    Some(node_proto::volume_block::Body::ZeroLength(n))
                                }
                                None => None,
                            },
                        },
                    )),
                };
                if tx.send(mapped).await.is_err() {
                    break;
                }
            }
        });
        let outbound = ReceiverStream::new(rx);
        let resp = storage
            .write_volume_extents(outbound)
            .await
            .map_err(|e| Status::internal(format!("write volume blocks: {e}")))?
            .into_inner();
        Ok(Response::new(controller_proto::WriteVolumeBlocksResponse {
            bytes_written: resp.bytes_written,
            blocks_written: resp.blocks_written,
            zero_runs: resp.zero_runs,
        }))
    }

    async fn freeze_vm_filesystems(
        &self,
        request: Request<controller_proto::FreezeVmFilesystemsRequest>,
    ) -> Result<Response<controller_proto::FreezeVmFilesystemsResponse>, Status> {
        self.require_operator(&request, OperatorRole::BackupOperator)?;
        let req = request.into_inner();
        let vm = self.resolve_vm(req.vm_name.trim())?;
        self.guest_fs_freeze_for_vm(&vm, req.freeze, &req.pre_script, &req.post_script)
            .await?;
        Ok(Response::new(
            controller_proto::FreezeVmFilesystemsResponse {
                success: true,
                message: if req.freeze {
                    "frozen".into()
                } else {
                    "thawed".into()
                },
            },
        ))
    }

    async fn prepare_instant_recovery(
        &self,
        request: Request<controller_proto::PrepareInstantRecoveryRequest>,
    ) -> Result<Response<controller_proto::PrepareInstantRecoveryResponse>, Status> {
        self.require_operator(&request, OperatorRole::BackupOperator)?;
        let req = request.into_inner();
        if req.nbd_url.trim().is_empty() {
            return Err(Status::invalid_argument("nbd_url is required"));
        }
        let node = self.node_for_instant(req.node.trim())?;
        if self.clients.get_storage(&node.address).is_none() {
            let _ = self.clients.connect(&node.address).await;
        }
        let mut storage = self.clients.get_storage(&node.address).ok_or_else(|| {
            Status::failed_precondition(format!("storage client for {} unavailable", node.id))
        })?;
        let prepared = storage
            .prepare_instant_disk(node_proto::PrepareInstantDiskRequest {
                nbd_url: req.nbd_url,
                vm_name: req.vm_name,
                memory_bytes: req.memory_bytes,
                vcpus: req.vcpus,
            })
            .await?
            .into_inner();
        Ok(Response::new(
            controller_proto::PrepareInstantRecoveryResponse {
                success: prepared.success,
                message: prepared.message,
                disk_path: prepared.disk_path,
            },
        ))
    }

    async fn finalize_instant_recovery(
        &self,
        request: Request<controller_proto::FinalizeInstantRecoveryRequest>,
    ) -> Result<Response<controller_proto::FinalizeInstantRecoveryResponse>, Status> {
        self.require_operator(&request, OperatorRole::BackupOperator)?;
        let req = request.into_inner();
        let vol = self
            .resolve_volume_row(req.volume_name.trim())?
            .ok_or_else(|| Status::not_found(format!("volume '{}' not found", req.volume_name)))?;
        let node = self.node_for_instant(req.node.trim())?;
        if self.clients.get_storage(&node.address).is_none() {
            let _ = self.clients.connect(&node.address).await;
        }
        let mut storage = self.clients.get_storage(&node.address).ok_or_else(|| {
            Status::failed_precondition(format!("storage client for {} unavailable", node.id))
        })?;
        let backend = if vol.pool.is_empty() {
            vol.image.clone()
        } else {
            format!("{}/{}", vol.pool, vol.image)
        };
        let finished = storage
            .finalize_instant_disk(node_proto::FinalizeInstantDiskRequest {
                overlay_path: req.overlay_path,
                backend_handle: backend,
                storage_class: vol.storage_class.clone(),
                vm_name: req.vm_name,
            })
            .await?
            .into_inner();
        Ok(Response::new(
            controller_proto::FinalizeInstantRecoveryResponse {
                success: finished.success,
                message: finished.message,
                bytes_written: finished.bytes_written,
                volume_path: finished.volume_path,
            },
        ))
    }

    async fn encrypt_volume(
        &self,
        request: Request<controller_proto::EncryptVolumeRequest>,
    ) -> Result<Response<controller_proto::EncryptVolumeResponse>, Status> {
        self.require_operator(&request, OperatorRole::VmAdmin)?;
        let actor = Self::audit_actor(&request);
        let req = request.into_inner();
        if !req.destroy_data {
            return Err(Status::invalid_argument(
                "in-place encryption formats LUKS on the RBD image and destroys all existing \
                 plaintext data; offline migration is not supported — set destroy_data=true \
                 to acknowledge",
            ));
        }
        let mut vol = self
            .resolve_volume_row(req.name.trim())?
            .ok_or_else(|| Status::not_found(format!("volume '{}' not found", req.name)))?;
        crate::host_volume::require_ceph(&vol.storage_class, "encrypt")
            .map_err(Status::failed_precondition)?;
        if vol.encrypted {
            return Err(Status::failed_precondition(format!(
                "volume '{}' is already encrypted",
                vol.name
            )));
        }
        let mut push_node: Option<NodeRow> = None;
        if !vol.vm_id.is_empty() {
            let vm = self
                .db
                .get_vm(&vol.vm_id)
                .map_err(|e| Status::internal(e.to_string()))?
                .ok_or_else(|| Status::failed_precondition("attached VM missing"))?;
            self.require_vm_stopped_for_volume_change(&vm).await?;
            let node = self
                .db
                .get_node(&vm.node_id)
                .map_err(|e| Status::internal(e.to_string()))?
                .ok_or_else(|| Status::failed_precondition("VM node missing"))?;
            self.cold_release_ceph_vm(&vm, &node).await?;
            push_node = Some(node);
        } else if vol.is_attached() {
            return Err(Status::failed_precondition(
                "volume must be detached (or its VM stopped) before in-place encryption",
            ));
        }
        let node = self
            .pick_healthy_ceph_node()
            .await
            .map_err(Status::failed_precondition)?;
        if self.clients.get_storage(&node.address).is_none() {
            self.clients
                .connect(&node.address)
                .await
                .map_err(|e| Status::unavailable(format!("connecting to Ceph node: {e}")))?;
        }
        let mut storage = self
            .clients
            .get_storage(&node.address)
            .ok_or_else(|| Status::unavailable("Ceph storage client unavailable"))?;
        let master = crate::volume_crypto::MasterKeyStore::default()
            .load_or_create()
            .map_err(|e| Status::failed_precondition(format!("volume master key: {e}")))?;
        let dek = crate::volume_crypto::generate_dek()
            .map_err(|e| Status::internal(format!("generate dek: {e}")))?;
        let mapper = crate::volume_crypto::mapper_name_for_serial(&vol.serial);
        storage
            .format_encrypted_volume(node_proto::FormatEncryptedVolumeRequest {
                rbd_device: format!("{}/{}", vol.pool, vol.image),
                dek: dek.clone(),
                mapper_name: mapper.clone(),
            })
            .await
            .map_err(|e| Status::internal(format!("format encrypted volume: {e}")))?;
        let _ = storage
            .lock_encrypted_volume(node_proto::LockEncryptedVolumeRequest {
                mapper_name: mapper,
            })
            .await;
        vol.encrypted = true;
        vol.wrapped_dek = crate::volume_crypto::wrap_dek(&master, &dek)
            .map_err(|e| Status::internal(format!("wrap dek: {e}")))?;
        vol.guest_visible_bytes = -1;
        vol.guest_checked_at.clear();
        vol.generation += 1;
        self.db
            .upsert_volume(&vol)
            .map_err(|e| Status::internal(e.to_string()))?;
        if let Some(node) = push_node {
            self.push_config_and_await_apply(&node).await?;
        }
        self.log_replication_event(
            &actor,
            Some("volume.encrypt"),
            EVT_VOLUME_ENCRYPT,
            &format!("volume/{}", vol.name),
            serde_json::json!({
                "id": vol.id,
                "name": vol.name,
                "destroyDataAcknowledged": true,
            }),
        );
        let vm = if vol.vm_id.is_empty() {
            None
        } else {
            self.db
                .get_vm(&vol.vm_id)
                .map_err(|e| Status::internal(e.to_string()))?
        };
        let message = format!(
            "encrypted volume '{}' (LUKS formatted on RBD — prior plaintext data was destroyed)",
            vol.name
        );
        Ok(Response::new(controller_proto::EncryptVolumeResponse {
            success: true,
            message,
            volume: Some(self.volume_info_from_row(&vol, vm.as_ref())),
        }))
    }

    async fn resize_volume(
        &self,
        request: Request<controller_proto::ResizeVolumeRequest>,
    ) -> Result<Response<controller_proto::ResizeVolumeResponse>, Status> {
        self.require_operator(&request, OperatorRole::VmAdmin)?;
        let actor = Self::audit_actor(&request);
        let req = request.into_inner();
        let mut vol = self
            .resolve_volume_row(req.name.trim())?
            .ok_or_else(|| Status::not_found(format!("volume '{}' not found", req.name)))?;
        if crate::host_volume::is_host(&vol.storage_class) {
            return self
                .resize_host_volume(actor, vol, req.size_bytes, req.allow_shrink)
                .await;
        }
        let shrinking = req.size_bytes < vol.size_bytes;
        if req.size_bytes == vol.size_bytes {
            return Err(Status::invalid_argument(
                "size_bytes equals current provisioned size; no change",
            ));
        }
        if shrinking {
            if !req.allow_shrink {
                return Err(Status::invalid_argument(
                    "shrink refused; pass allow_shrink=true to shrink a detached volume via \
                     rbd resize --allow-shrink (destroys the tail of the image)",
                ));
            }
            if !vol.vm_id.is_empty() {
                let vm = self
                    .db
                    .get_vm(&vol.vm_id)
                    .map_err(|e| Status::internal(e.to_string()))?
                    .ok_or_else(|| Status::failed_precondition("attached VM missing"))?;
                if self.vm_is_live(&vm).await? {
                    return Err(Status::failed_precondition(format!(
                        "refusing shrink while VM '{}' is running; stop the VM and detach the \
                         volume first",
                        vm.name
                    )));
                }
                return Err(Status::failed_precondition(
                    "shrink requires the volume to be detached (detach data volumes before \
                     shrinking)",
                ));
            }
            if vol.is_attached() {
                return Err(Status::failed_precondition(
                    "shrink requires the volume to be detached",
                ));
            }
        } else if req.size_bytes <= vol.size_bytes {
            return Err(Status::invalid_argument(
                "size_bytes must be greater than the current provisioned size (or use \
                 allow_shrink to shrink)",
            ));
        }
        if !vol.vm_id.is_empty() {
            if let Ok(Some(open)) = self.db.get_open_vm_operation(&vol.vm_id) {
                return Err(Status::failed_precondition(format!(
                    "refusing resize while VM has open operation {} ({})",
                    open.id, open.phase
                )));
            }
        }
        let op_id = if vol.vm_id.is_empty() {
            String::new()
        } else {
            let id = Uuid::new_v4().to_string();
            self.db
                .insert_vm_operation(&VmOperationRow {
                    id: id.clone(),
                    vm_id: vol.vm_id.clone(),
                    kind: "resize".into(),
                    phase: VmOperationRow::PHASE_PREPARING.into(),
                    source_node: String::new(),
                    target_node: String::new(),
                    cancel_requested: false,
                    send_succeeded: false,
                    detail_json: format!(
                        r#"{{"volume":"{}","size":{}}}"#,
                        vol.name, req.size_bytes
                    ),
                    started_at: String::new(),
                    updated_at: String::new(),
                    finished_at: String::new(),
                })
                .map_err(|e| Status::failed_precondition(e.to_string()))?;
            id
        };
        let node = self
            .pick_healthy_ceph_node()
            .await
            .map_err(Status::failed_precondition)?;
        if self.clients.get_storage(&node.address).is_none() {
            let _ = self.clients.connect(&node.address).await;
        }
        let mut storage = self
            .clients
            .get_storage(&node.address)
            .ok_or_else(|| Status::unavailable("Ceph storage client unavailable"))?;
        let resized = storage
            .resize_volume(node_proto::ResizeVolumeRequest {
                backend_handle: format!("{}/{}", vol.pool, vol.image),
                size_bytes: req.size_bytes,
                allow_shrink: shrinking,
            })
            .await
            .map_err(|e| {
                if !op_id.is_empty() {
                    let _ = self.db.update_vm_operation_phase(
                        &op_id,
                        VmOperationRow::PHASE_FAILED,
                        None,
                        None,
                        true,
                    );
                }
                Status::internal(format!("rbd resize: {e}"))
            })?
            .into_inner();
        vol.size_bytes = if resized.size_bytes > 0 {
            resized.size_bytes
        } else {
            req.size_bytes
        };
        vol.guest_visible_bytes = -1;
        vol.guest_checked_at.clear();
        vol.generation += 1;
        self.db
            .upsert_volume(&vol)
            .map_err(|e| Status::internal(e.to_string()))?;
        if vol.encrypted && !vol.vm_id.is_empty() {
            if let Err(e) = self.grow_encrypted_mapper(&vol).await {
                if !op_id.is_empty() {
                    let _ = self.db.update_vm_operation_phase(
                        &op_id,
                        VmOperationRow::PHASE_FAILED,
                        None,
                        None,
                        true,
                    );
                }
                return Err(e);
            }
        }
        if !op_id.is_empty() {
            let _ = self.db.update_vm_operation_phase(
                &op_id,
                VmOperationRow::PHASE_DONE,
                None,
                None,
                true,
            );
        }
        let vm = if vol.vm_id.is_empty() {
            None
        } else {
            self.db
                .get_vm(&vol.vm_id)
                .map_err(|e| Status::internal(e.to_string()))?
        };
        let guest_status_note = if vol.role == VolumeRow::ROLE_ROOT {
            "Cloud Hypervisor has no vm.resize-disk; the root disk cannot be hot-removed. \
             guestVisibleBytes may remain unknown until the VM reboots (data volumes may \
             hot-replug)."
                .into()
        } else {
            String::new()
        };
        let mut message = if shrinking {
            let mut m = format!(
                "shrank '{}' to {} bytes via rbd resize --allow-shrink",
                vol.name, vol.size_bytes
            );
            if vol.guest_visible_bytes >= 0 && req.size_bytes < vol.guest_visible_bytes {
                m.push_str("; new size is below last observed guest visible size — tail destroyed");
            }
            m
        } else {
            format!(
                "resized '{}' to {} bytes (guestVisibleBytes still unknown until next check)",
                vol.name, vol.size_bytes
            )
        };
        if !guest_status_note.is_empty() && !shrinking {
            message.push_str(&format!("; {guest_status_note}"));
        }
        if let Some(ref vm_row) = vm {
            if vm_row.auto_start {
                if let Ok(Some(node)) = self.db.get_node(&vm_row.node_id) {
                    if self.clients.get_admin(&node.address).is_none() {
                        let _ = self.clients.connect(&node.address).await;
                    }
                    if let Some(mut admin) = self.clients.get_admin(&node.address) {
                        let runtime_name = sanitize_nix_attr_key(&vm_row.name);
                        let disk_serial = guest_virtio_serial(&vol, &runtime_name);
                        let guest_result = if req.grow_filesystem {
                            admin
                                .guest_grow_filesystem(node_proto::GuestGrowFilesystemRequest {
                                    vm_name: runtime_name.clone(),
                                    network: vm_row.network.clone(),
                                    disk_serial: disk_serial.clone(),
                                    ssh_user: String::new(),
                                    port: 22,
                                    timeout_ms: 60_000,
                                })
                                .await
                                .map(|r| {
                                    let i = r.into_inner();
                                    (i.success, i.message, i.size_bytes, i.device)
                                })
                        } else {
                            admin
                                .guest_disk_probe(node_proto::GuestDiskProbeRequest {
                                    vm_name: runtime_name,
                                    network: vm_row.network.clone(),
                                    disk_serial,
                                    ssh_user: String::new(),
                                    port: 22,
                                    timeout_ms: 30_000,
                                })
                                .await
                                .map(|r| {
                                    let i = r.into_inner();
                                    (i.success, i.message, i.size_bytes, i.device)
                                })
                        };
                        match guest_result {
                            Ok((success, gmsg, size_bytes, device)) => {
                                if success && size_bytes > 0 {
                                    vol.guest_visible_bytes = size_bytes;
                                    vol.guest_checked_at = chrono_like_now();
                                    let _ = self.db.upsert_volume(&vol);
                                    message = format!(
                                        "resized '{}' to {} bytes; guest sees {} bytes on {} ({})",
                                        vol.name,
                                        vol.size_bytes,
                                        size_bytes,
                                        device,
                                        if req.grow_filesystem {
                                            "grew"
                                        } else {
                                            "probed"
                                        }
                                    );
                                } else {
                                    message.push_str(&format!(
                                        "; GuestOps: {}",
                                        if gmsg.is_empty() {
                                            "no size reported"
                                        } else {
                                            &gmsg
                                        }
                                    ));
                                }
                            }
                            Err(e) => {
                                message.push_str(&format!("; GuestOps unavailable: {e}"));
                            }
                        }
                    }
                }
            }
        }
        Ok(Response::new(controller_proto::ResizeVolumeResponse {
            success: true,
            message,
            volume: Some(self.volume_info_from_row(&vol, vm.as_ref())),
            operation_id: op_id,
            guest_status_note,
        }))
    }

    async fn create_snapshot_policy(
        &self,
        request: Request<controller_proto::CreateSnapshotPolicyRequest>,
    ) -> Result<Response<controller_proto::CreateSnapshotPolicyResponse>, Status> {
        self.require_operator(&request, OperatorRole::VmAdmin)?;
        let policy = request
            .into_inner()
            .policy
            .ok_or_else(|| Status::invalid_argument("policy is required"))?;
        let name = policy.name.trim();
        if name.is_empty() {
            return Err(Status::invalid_argument("policy name is required"));
        }
        if policy.keep <= 0 {
            return Err(Status::invalid_argument("keep must be positive"));
        }
        if policy.selector_vm.trim().is_empty() && policy.selector_volume.trim().is_empty() {
            return Err(Status::invalid_argument(
                "selector_vm or selector_volume is required",
            ));
        }
        let existing = self
            .db
            .get_snapshot_policy(name)
            .map_err(|e| Status::internal(e.to_string()))?;
        // Proto3 bool defaults to false; on first create treat unset as enabled.
        let enabled = if existing.is_some() {
            policy.enabled
        } else {
            true
        };
        let row = SnapshotPolicyRow {
            name: name.into(),
            selector_vm: policy.selector_vm.trim().into(),
            selector_volume: policy.selector_volume.trim().into(),
            schedule: if policy.schedule.trim().is_empty() {
                "@daily".into()
            } else {
                policy.schedule.trim().into()
            },
            keep: policy.keep,
            enabled,
            last_run_at: existing
                .as_ref()
                .map(|e| e.last_run_at.clone())
                .unwrap_or_default(),
            last_message: existing
                .as_ref()
                .map(|e| e.last_message.clone())
                .unwrap_or_default(),
            created_at: existing
                .as_ref()
                .map(|e| e.created_at.clone())
                .unwrap_or_default(),
            updated_at: String::new(),
        };
        self.db
            .upsert_snapshot_policy(&row)
            .map_err(|e| Status::internal(e.to_string()))?;
        Ok(Response::new(
            controller_proto::CreateSnapshotPolicyResponse {
                success: true,
                policy: Some(Self::snapshot_policy_to_proto(&row)),
                message: format!("snapshot policy '{name}' upserted"),
            },
        ))
    }

    async fn get_snapshot_policy(
        &self,
        request: Request<controller_proto::GetSnapshotPolicyRequest>,
    ) -> Result<Response<controller_proto::GetSnapshotPolicyResponse>, Status> {
        self.require_operator(&request, OperatorRole::ReadOnly)?;
        let name = request.into_inner().name;
        let row = self
            .db
            .get_snapshot_policy(name.trim())
            .map_err(|e| Status::internal(e.to_string()))?
            .ok_or_else(|| Status::not_found(format!("snapshot policy '{name}' not found")))?;
        Ok(Response::new(controller_proto::GetSnapshotPolicyResponse {
            policy: Some(Self::snapshot_policy_to_proto(&row)),
        }))
    }

    async fn list_snapshot_policies(
        &self,
        request: Request<controller_proto::ListSnapshotPoliciesRequest>,
    ) -> Result<Response<controller_proto::ListSnapshotPoliciesResponse>, Status> {
        self.require_operator(&request, OperatorRole::ReadOnly)?;
        let _ = request.into_inner();
        let policies = self
            .db
            .list_snapshot_policies()
            .map_err(|e| Status::internal(e.to_string()))?
            .iter()
            .map(Self::snapshot_policy_to_proto)
            .collect();
        Ok(Response::new(
            controller_proto::ListSnapshotPoliciesResponse { policies },
        ))
    }

    async fn delete_snapshot_policy(
        &self,
        request: Request<controller_proto::DeleteSnapshotPolicyRequest>,
    ) -> Result<Response<controller_proto::DeleteSnapshotPolicyResponse>, Status> {
        self.require_operator(&request, OperatorRole::VmAdmin)?;
        let name = request.into_inner().name;
        if !self
            .db
            .delete_snapshot_policy(name.trim())
            .map_err(|e| Status::internal(e.to_string()))?
        {
            return Err(Status::not_found(format!(
                "snapshot policy '{name}' not found"
            )));
        }
        Ok(Response::new(
            controller_proto::DeleteSnapshotPolicyResponse {
                success: true,
                message: format!("deleted snapshot policy '{name}'"),
            },
        ))
    }

    async fn get_compliance_report(
        &self,
        request: Request<controller_proto::GetComplianceReportRequest>,
    ) -> Result<Response<controller_proto::GetComplianceReportResponse>, Status> {
        self.require_operator(&request, OperatorRole::ReadOnly)?;

        let (approved, pending, rejected) = self
            .db
            .count_nodes_by_approval()
            .map_err(|e| Status::internal(e.to_string()))?;
        let total_nodes = approved + pending + rejected;

        let (total_vms, running_vms) = self
            .db
            .count_vms_by_auto_start()
            .map_err(|e| Status::internal(e.to_string()))?;
        let stopped_vms = total_vms - running_vms;

        let (nat, bridge, vxlan) = self
            .db
            .count_networks_by_type()
            .map_err(|e| Status::internal(e.to_string()))?;
        let total_networks = nat + bridge + vxlan;

        let (expiring_30d, cert_unknown) = self
            .db
            .count_nodes_cert_expiry()
            .map_err(|e| Status::internal(e.to_string()))?;

        let (luks_tpm2, luks_keyfile, luks_unknown) = self
            .db
            .count_nodes_luks_method()
            .map_err(|e| Status::internal(e.to_string()))?;

        let sub_ca_enabled = self.sub_ca.lock().unwrap().is_available();

        let node_rows = self
            .db
            .list_nodes()
            .map_err(|e| Status::internal(e.to_string()))?;
        let all_labels = self.db.get_all_node_labels().unwrap_or_default();
        let nodes: Vec<controller_proto::NodeInfo> = node_rows
            .into_iter()
            .map(|n| {
                let labels: Vec<String> = all_labels
                    .iter()
                    .filter(|(nid, _)| nid == &n.id)
                    .map(|(_, l)| l.clone())
                    .collect();
                let hb = if n.last_heartbeat.is_empty() {
                    None
                } else {
                    parse_datetime_to_timestamp(&n.last_heartbeat)
                };
                let (
                    kcore_version,
                    booted_generation,
                    nixos_generation,
                    update_capabilities,
                    schema_version,
                    nixpkgs_rev,
                ) = host_status_fields(&self.db, &n.id);
                controller_proto::NodeInfo {
                    node_id: n.id,
                    hostname: n.hostname,
                    address: n.address,
                    capacity: Some(controller_proto::NodeCapacity {
                        cpu_cores: n.cpu_cores,
                        memory_bytes: n.memory_bytes,
                    }),
                    usage: Some(controller_proto::NodeUsage {
                        cpu_cores_used: n.cpu_used,
                        memory_bytes_used: n.memory_used,
                    }),
                    status: n.status,
                    last_heartbeat: hb,
                    labels,
                    storage_backend: storage_backend_to_proto(&n.storage_backend),
                    disable_vxlan: n.disable_vxlan,
                    approval_status: n.approval_status,
                    cert_expiry_days: n.cert_expiry_days,
                    luks_method: n.luks_method,
                    dc_id: n.dc_id,
                    kcore_version,
                    booted_generation,
                    nixos_generation,
                    update_capabilities,
                    schema_version,
                    nixpkgs_rev,
                }
            })
            .collect();

        let access_control = rbac_matrix::compliance_access_control_entries();

        Ok(Response::new(
            controller_proto::GetComplianceReportResponse {
                controller_version: env!("CARGO_PKG_VERSION").to_string(),
                crypto_library: crate::crypto_profile::CRYPTO_LIBRARY.into(),
                tls13_cipher_suites: crate::crypto_profile::tls13_cipher_suites(),
                tls12_cipher_suites: crate::crypto_profile::tls12_cipher_suites(),
                kx_groups: crate::crypto_profile::kx_groups(),
                excluded_algorithms: crate::crypto_profile::excluded_algorithms(),
                mtls_enabled: self.tls_paths.is_some(),
                access_control,
                total_nodes,
                approved_nodes: approved,
                pending_nodes: pending,
                rejected_nodes: rejected,
                total_vms,
                running_vms,
                stopped_vms,
                total_networks,
                nat_networks: nat,
                bridge_networks: bridge,
                vxlan_networks: vxlan,
                sub_ca_enabled,
                cert_auto_renewal_days: 30,
                nodes_expiring_30d: expiring_30d,
                nodes_cert_unknown: cert_unknown,
                nodes,
                nodes_luks_tpm2: luks_tpm2,
                nodes_luks_keyfile: luks_keyfile,
                nodes_luks_unknown: luks_unknown,
            },
        ))
    }

    async fn list_audit_events(
        &self,
        request: Request<controller_proto::ListAuditEventsRequest>,
    ) -> Result<Response<controller_proto::ListAuditEventsResponse>, Status> {
        self.require_operator(&request, OperatorRole::ReadOnly)?;
        let req = request.into_inner();
        let since = {
            let s = req.since.trim();
            if s.is_empty() {
                None
            } else {
                Some(s.to_string())
            }
        };
        let action = {
            let a = req.action.trim();
            if a.is_empty() {
                None
            } else {
                Some(a.to_string())
            }
        };
        let rows = self
            .db
            .list_audit_events(req.limit, since.as_deref(), action.as_deref())
            .map_err(|e| Status::internal(format!("listing audit events: {e}")))?;
        let events = rows
            .into_iter()
            .map(|r| controller_proto::AuditEvent {
                id: r.id,
                actor: r.actor,
                action: r.action,
                resource: r.resource,
                created_at: r.created_at,
                detail: r.detail,
            })
            .collect();
        Ok(Response::new(controller_proto::ListAuditEventsResponse {
            events,
        }))
    }

    async fn get_crypto_config(
        &self,
        request: Request<controller_proto::GetCryptoConfigRequest>,
    ) -> Result<Response<controller_proto::GetCryptoConfigResponse>, Status> {
        self.require_operator(&request, OperatorRole::ReadOnly)?;
        Ok(Response::new(controller_proto::GetCryptoConfigResponse {
            controller_version: env!("CARGO_PKG_VERSION").to_string(),
            crypto_library: crate::crypto_profile::CRYPTO_LIBRARY.into(),
            fips_certificate: crate::crypto_profile::FIPS_CERTIFICATE.into(),
            tls13_cipher_suites: crate::crypto_profile::tls13_cipher_suites(),
            tls12_cipher_suites: crate::crypto_profile::tls12_cipher_suites(),
            kx_groups: crate::crypto_profile::kx_groups(),
            excluded_algorithms: crate::crypto_profile::excluded_algorithms(),
            mtls_enabled: self.tls_paths.is_some(),
            rate_limit_enabled: self.rate_limit.enabled,
            rate_limit_requests_per_second: self.rate_limit.requests_per_second,
            rate_limit_burst: self.rate_limit.burst,
            revocation_fail_mode: self.revocation_fail_mode.clone(),
            signing_scheme: crate::crypto_profile::SIGNING_SCHEME.into(),
        }))
    }

    async fn export_sbom(
        &self,
        request: Request<controller_proto::ExportSbomRequest>,
    ) -> Result<Response<controller_proto::ExportSbomResponse>, Status> {
        self.require_operator(&request, OperatorRole::ReadOnly)?;
        let kind = request.into_inner().kind;
        let doc = crate::sbom::load(&kind, &self.sbom)?;
        Ok(Response::new(controller_proto::ExportSbomResponse {
            format: doc.format,
            spec_version: doc.spec_version,
            filename: doc.filename,
            sha256: doc.sha256,
            document: doc.document,
            source: doc.source,
            generator: doc.generator,
            signature_present: doc.signature_present,
            signing_scheme: doc.signing_scheme,
        }))
    }

    async fn create_disk_layout(
        &self,
        request: Request<controller_proto::CreateDiskLayoutRequest>,
    ) -> Result<Response<controller_proto::CreateDiskLayoutResponse>, Status> {
        self.require_operator(&request, OperatorRole::ClusterAdmin)?;
        let actor = Self::audit_actor(&request);
        self.create_disk_layout_impl(&actor, request.into_inner())
            .await
    }

    async fn get_disk_layout(
        &self,
        request: Request<controller_proto::GetDiskLayoutRequest>,
    ) -> Result<Response<controller_proto::GetDiskLayoutResponse>, Status> {
        self.require_operator(&request, OperatorRole::ReadOnly)?;
        self.get_disk_layout_impl(request.into_inner()).await
    }

    async fn list_disk_layouts(
        &self,
        request: Request<controller_proto::ListDiskLayoutsRequest>,
    ) -> Result<Response<controller_proto::ListDiskLayoutsResponse>, Status> {
        self.require_operator(&request, OperatorRole::ReadOnly)?;
        self.list_disk_layouts_impl(request.into_inner()).await
    }

    async fn delete_disk_layout(
        &self,
        request: Request<controller_proto::DeleteDiskLayoutRequest>,
    ) -> Result<Response<controller_proto::DeleteDiskLayoutResponse>, Status> {
        self.require_operator(&request, OperatorRole::ClusterAdmin)?;
        let actor = Self::audit_actor(&request);
        self.delete_disk_layout_impl(&actor, request.into_inner())
            .await
    }

    async fn plan_ceph_cluster(
        &self,
        request: Request<controller_proto::PlanCephClusterRequest>,
    ) -> Result<Response<controller_proto::PlanCephClusterResponse>, Status> {
        self.require_operator(&request, OperatorRole::ClusterAdmin)?;
        let incoming = request
            .into_inner()
            .ceph_cluster
            .ok_or_else(|| Status::invalid_argument("ceph_cluster is required"))?;
        let mut spec = incoming
            .spec
            .ok_or_else(|| Status::invalid_argument("spec is required"))?;
        if spec.fsid.trim().is_empty() {
            spec.fsid = "00000000-0000-0000-0000-000000000000".into();
        }
        let plan = self.plan_ceph_cluster_spec(&incoming.name, &spec).await?;
        Ok(Response::new(plan))
    }

    async fn create_ceph_cluster(
        &self,
        request: Request<controller_proto::CreateCephClusterRequest>,
    ) -> Result<Response<controller_proto::CreateCephClusterResponse>, Status> {
        self.require_operator(&request, OperatorRole::ClusterAdmin)?;
        let incoming = request
            .into_inner()
            .ceph_cluster
            .ok_or_else(|| Status::invalid_argument("ceph_cluster is required"))?;
        let name = validate_network_name(&incoming.name)?;
        let mut spec = incoming
            .spec
            .ok_or_else(|| Status::invalid_argument("spec is required"))?;
        if spec.fsid.trim().is_empty() {
            if let Some(existing) = self
                .db
                .get_ceph_cluster(&name)
                .map_err(|e| Status::internal(e.to_string()))?
            {
                if let Ok(prev) = ceph_cluster_spec::spec_from_json(&existing.spec_json) {
                    if !prev.fsid.trim().is_empty() {
                        spec.fsid = prev.fsid;
                    }
                }
            }
            if spec.fsid.trim().is_empty() {
                spec.fsid = Uuid::new_v4().to_string();
            }
        }
        ceph_cluster_spec::validate_spec(&spec).map_err(Status::invalid_argument)?;
        for node in &spec.nodes {
            if self
                .db
                .get_node(&node.node_id)
                .map_err(|e| Status::internal(e.to_string()))?
                .is_none()
            {
                return Err(Status::not_found(format!(
                    "node '{}' is not registered",
                    node.node_id
                )));
            }
        }
        let spec_json = ceph_cluster_spec::spec_to_json(&spec)
            .map_err(|e| Status::internal(format!("encode ceph spec: {e}")))?;
        let existing = self
            .db
            .get_ceph_cluster(&name)
            .map_err(|e| Status::internal(e.to_string()))?;
        let (action, generation, changed_fields) = match existing.as_ref() {
            Some(e) if e.spec_json == spec_json => (
                controller_proto::ApplyAction::Unchanged as i32,
                e.generation,
                vec![],
            ),
            Some(e) => (
                controller_proto::ApplyAction::Updated as i32,
                e.generation.saturating_add(1),
                vec!["spec".to_string()],
            ),
            None => (controller_proto::ApplyAction::Created as i32, 1, vec![]),
        };
        // Preflight before any mutation (skip when the apply is a no-op).
        if action != controller_proto::ApplyAction::Unchanged as i32 {
            let plan = self.plan_ceph_cluster_spec(&name, &spec).await?;
            if !plan.ok {
                let mut lines = vec![plan.encrypt_osds_notice];
                for n in &plan.nodes {
                    if !n.ok {
                        lines.push(format!("node {}: {}", n.node_id, n.issues.join("; ")));
                    }
                }
                for c in &plan.cluster_issues {
                    lines.push(c.clone());
                }
                return Err(Status::failed_precondition(format!(
                    "CephCluster preflight failed:\n{}",
                    lines.join("\n")
                )));
            }
        }
        if let Some(prev) = existing.as_ref() {
            if prev.spec_json != spec_json {
                if let Ok(old) = ceph_cluster_spec::spec_from_json(&prev.spec_json) {
                    if old.encrypt_osds != spec.encrypt_osds {
                        return Err(Status::failed_precondition(
                            "encryptOsds is immutable after the CephCluster exists; rotate encryption by replacing OSD devices",
                        ));
                    }
                    for node in &old.nodes {
                        if !spec.nodes.iter().any(|n| n.node_id == node.node_id) {
                            self.db
                                .add_retired_ceph_node(&name, &node.node_id)
                                .map_err(|e| Status::internal(e.to_string()))?;
                        }
                    }
                    for node in &spec.nodes {
                        self.db
                            .clear_retired_ceph_node(&name, &node.node_id)
                            .map_err(|e| Status::internal(e.to_string()))?;
                    }
                }
            }
        }
        let row = if action == controller_proto::ApplyAction::Unchanged as i32 {
            existing.expect("unchanged requires existing")
        } else {
            let row = CephClusterRow {
                name: name.clone(),
                generation,
                spec_json,
                bootstrap_json: existing
                    .as_ref()
                    .map(|e| e.bootstrap_json.clone())
                    .unwrap_or_default(),
                created_at: existing
                    .as_ref()
                    .map(|e| e.created_at.clone())
                    .unwrap_or_default(),
                updated_at: String::new(),
            };
            self.db
                .upsert_ceph_cluster(&row)
                .map_err(|e| Status::internal(e.to_string()))?;
            self.db
                .upsert_ceph_cluster_status(&CephClusterStatusRow {
                    name: name.clone(),
                    observed_generation: 0,
                    phase: "pending".into(),
                    health_message: String::new(),
                    ceph_status_json: String::new(),
                    last_transition_at: String::new(),
                })
                .map_err(|e| Status::internal(e.to_string()))?;
            self.db
                .get_ceph_cluster(&name)
                .map_err(|e| Status::internal(e.to_string()))?
                .unwrap()
        };
        let status = self
            .db
            .get_ceph_cluster_status(&name)
            .map_err(|e| Status::internal(e.to_string()))?;
        Ok(Response::new(controller_proto::CreateCephClusterResponse {
            success: true,
            ceph_cluster: Some(ceph_cluster_to_proto(&row, status)?),
            action,
            changed_fields,
        }))
    }

    async fn get_ceph_cluster(
        &self,
        request: Request<controller_proto::GetCephClusterRequest>,
    ) -> Result<Response<controller_proto::GetCephClusterResponse>, Status> {
        self.require_operator(&request, OperatorRole::ReadOnly)?;
        let name = request.into_inner().name;
        let row = self
            .db
            .get_ceph_cluster(name.trim())
            .map_err(|e| Status::internal(e.to_string()))?
            .ok_or_else(|| Status::not_found(format!("ceph cluster '{}' not found", name)))?;
        let status = self
            .db
            .get_ceph_cluster_status(name.trim())
            .map_err(|e| Status::internal(e.to_string()))?;
        Ok(Response::new(controller_proto::GetCephClusterResponse {
            ceph_cluster: Some(ceph_cluster_to_proto(&row, status)?),
        }))
    }

    async fn list_ceph_clusters(
        &self,
        request: Request<controller_proto::ListCephClustersRequest>,
    ) -> Result<Response<controller_proto::ListCephClustersResponse>, Status> {
        self.require_operator(&request, OperatorRole::ReadOnly)?;
        let mut clusters = Vec::new();
        for row in self
            .db
            .list_ceph_clusters()
            .map_err(|e| Status::internal(e.to_string()))?
        {
            let status = self
                .db
                .get_ceph_cluster_status(&row.name)
                .map_err(|e| Status::internal(e.to_string()))?;
            clusters.push(ceph_cluster_to_proto(&row, status)?);
        }
        Ok(Response::new(controller_proto::ListCephClustersResponse {
            ceph_clusters: clusters,
        }))
    }

    async fn get_ceph_dashboard_credentials(
        &self,
        request: Request<controller_proto::GetCephDashboardCredentialsRequest>,
    ) -> Result<Response<controller_proto::GetCephDashboardCredentialsResponse>, Status> {
        self.require_operator(&request, OperatorRole::ClusterAdmin)?;
        let name = request.into_inner().name;
        let name = name.trim();
        if name.is_empty() {
            return Err(Status::invalid_argument("name is required"));
        }
        let row = self
            .db
            .get_ceph_dashboard_credentials(name)
            .map_err(|e| Status::internal(e.to_string()))?
            .ok_or_else(|| {
                Status::not_found(format!(
                    "no dashboard credentials stored for ceph cluster '{name}' \
                     (set with SetCephDashboardCredentials / \
                     `kctl set ceph-dashboard-credentials`)"
                ))
            })?;
        Ok(Response::new(
            controller_proto::GetCephDashboardCredentialsResponse {
                name: row.name,
                url: row.url,
                username: row.username,
                password: row.password,
                updated_at: row.updated_at,
            },
        ))
    }

    async fn set_ceph_dashboard_credentials(
        &self,
        request: Request<controller_proto::SetCephDashboardCredentialsRequest>,
    ) -> Result<Response<controller_proto::SetCephDashboardCredentialsResponse>, Status> {
        self.require_operator(&request, OperatorRole::ClusterAdmin)?;
        let actor = Self::audit_actor(&request);
        let req = request.into_inner();
        let name = req.name.trim().to_string();
        if name.is_empty() {
            return Err(Status::invalid_argument("name is required"));
        }
        let url = req.url.trim().to_string();
        let username = req.username.trim().to_string();
        let password = req.password;
        if url.is_empty() {
            return Err(Status::invalid_argument("url is required"));
        }
        if username.is_empty() {
            return Err(Status::invalid_argument("username is required"));
        }
        if password.is_empty() {
            return Err(Status::invalid_argument("password is required"));
        }
        self.db
            .upsert_ceph_dashboard_credentials(&crate::db::CephDashboardCredentialsRow {
                name: name.clone(),
                url: url.clone(),
                username: username.clone(),
                password: password.clone(),
                updated_at: String::new(),
            })
            .map_err(|e| Status::internal(e.to_string()))?;
        self.record_audit(
            &actor,
            "SetCephDashboardCredentials",
            &format!("ceph_cluster/{name}"),
            format!("url={url} user={username}"),
        );
        let stored = self
            .db
            .get_ceph_dashboard_credentials(&name)
            .map_err(|e| Status::internal(e.to_string()))?
            .ok_or_else(|| Status::internal("credentials missing after upsert"))?;
        Ok(Response::new(
            controller_proto::SetCephDashboardCredentialsResponse {
                success: true,
                credentials: Some(controller_proto::GetCephDashboardCredentialsResponse {
                    name: stored.name,
                    url: stored.url,
                    username: stored.username,
                    password: stored.password,
                    updated_at: stored.updated_at,
                }),
            },
        ))
    }

    async fn get_ceph_cluster_capacity(
        &self,
        request: Request<controller_proto::GetCephClusterCapacityRequest>,
    ) -> Result<Response<controller_proto::GetCephClusterCapacityResponse>, Status> {
        self.require_operator(&request, OperatorRole::ReadOnly)?;
        let name = request.into_inner().name;
        let name = name.trim();
        if name.is_empty() {
            return Err(Status::invalid_argument("name is required"));
        }
        let row = self
            .db
            .get_ceph_cluster(name)
            .map_err(|e| Status::internal(e.to_string()))?
            .ok_or_else(|| Status::not_found(format!("ceph cluster '{name}' not found")))?;
        let spec = ceph_cluster_spec::spec_from_json(&row.spec_json)
            .map_err(|e| Status::internal(format!("decode ceph spec: {e}")))?;
        if spec.nodes.is_empty() {
            return Ok(Response::new(
                controller_proto::GetCephClusterCapacityResponse {
                    ok: false,
                    message: "cluster has no members".into(),
                    ..Default::default()
                },
            ));
        }

        let mut last_err = String::new();
        for member in &spec.nodes {
            let Ok(Some(node)) = self.db.get_node(&member.node_id) else {
                last_err = format!("member {} is not registered", member.node_id);
                continue;
            };
            let Ok(mut admin) = self.ensure_admin_client_for_node(&node).await else {
                last_err = format!("cannot reach member {}", member.node_id);
                continue;
            };
            match admin
                .get_ceph_df(node_proto::GetCephDfRequest {
                    cluster_name: "ceph".into(),
                })
                .await
            {
                Ok(resp) => {
                    let df = resp.into_inner();
                    return Ok(Response::new(
                        controller_proto::GetCephClusterCapacityResponse {
                            ok: true,
                            message: String::new(),
                            queried_node: member.node_id.clone(),
                            total_bytes: df.total_bytes,
                            used_bytes: df.used_bytes,
                            avail_bytes: df.avail_bytes,
                            by_class: df
                                .by_class
                                .into_iter()
                                .map(|c| controller_proto::CephDeviceClassCapacity {
                                    device_class: c.device_class,
                                    total_bytes: c.total_bytes,
                                    used_bytes: c.used_bytes,
                                    avail_bytes: c.avail_bytes,
                                })
                                .collect(),
                            pools: df
                                .pools
                                .into_iter()
                                .map(|p| controller_proto::CephPoolCapacity {
                                    name: p.name,
                                    stored_bytes: p.stored_bytes,
                                    objects: p.objects,
                                    percent_used_bp: p.percent_used_bp,
                                    max_avail_bytes: p.max_avail_bytes,
                                })
                                .collect(),
                        },
                    ));
                }
                Err(e) => {
                    last_err = format!("{}: {e}", member.node_id);
                }
            }
        }

        Ok(Response::new(
            controller_proto::GetCephClusterCapacityResponse {
                ok: false,
                message: if last_err.is_empty() {
                    "no Ceph member answered".into()
                } else {
                    last_err
                },
                ..Default::default()
            },
        ))
    }

    async fn delete_ceph_cluster(
        &self,
        request: Request<controller_proto::DeleteCephClusterRequest>,
    ) -> Result<Response<controller_proto::DeleteCephClusterResponse>, Status> {
        self.require_operator(&request, OperatorRole::ClusterAdmin)?;
        let req = request.into_inner();
        let force = req.force;
        let name_owned = req.name.trim().to_string();
        let name = name_owned.as_str();
        // Deleting the CephCluster record strands every RBD-backed VM that
        // lives on its members: the reconciler stops managing the cluster, and
        // `node_supports_backend` stops recognising those nodes as Ceph-capable
        // so the VMs can no longer be created, migrated, or drained.
        let in_use = self.ceph_cluster_vms_in_use(name)?;
        if !in_use.is_empty() {
            return Err(Status::failed_precondition(format!(
                "CephCluster '{name}' still backs {} VM(s): {}. Delete those VMs first.",
                in_use.len(),
                in_use.join(", ")
            )));
        }
        if let Some(row) = self
            .db
            .get_ceph_cluster(name)
            .map_err(|e| Status::internal(e.to_string()))?
        {
            let mut members = Vec::new();
            if let Ok(spec) = ceph_cluster_spec::spec_from_json(&row.spec_json) {
                members.extend(spec.nodes.iter().map(|n| n.node_id.clone()));
            }
            if let Ok(retired) = self.db.list_retired_ceph_nodes(name) {
                for node_id in retired {
                    if !members.contains(&node_id) {
                        members.push(node_id);
                    }
                }
            }
            let mut teardown_errors = Vec::new();
            for node_id in &members {
                let Some(node) = self
                    .db
                    .get_node(node_id)
                    .map_err(|e| Status::internal(e.to_string()))?
                else {
                    teardown_errors.push(format!("{node_id} is not registered"));
                    continue;
                };
                let mut admin = match self.ensure_admin_client_for_node(&node).await {
                    Ok(admin) => admin,
                    Err(e) => {
                        teardown_errors.push(format!("{node_id}: {e}"));
                        continue;
                    }
                };
                match admin
                    .teardown_ceph_node(node_proto::TeardownCephNodeRequest {})
                    .await
                {
                    Ok(resp) => {
                        let resp = resp.into_inner();
                        if !resp.success {
                            teardown_errors.push(format!("{node_id}: {}", resp.message));
                            continue;
                        }
                        if !resp.apply_id.is_empty() {
                            if let Err(e) = self
                                .await_nix_apply(&node, &resp.apply_id, &mut admin)
                                .await
                            {
                                teardown_errors.push(format!("{node_id}: {e}"));
                            }
                        }
                    }
                    Err(e) => teardown_errors.push(format!("{node_id}: {e}")),
                }
            }
            if !teardown_errors.is_empty() && !force {
                return Err(Status::failed_precondition(format!(
                    "CephCluster '{name}' was not deleted; could not stop every member (pass force to delete the record anyway): {}",
                    teardown_errors.join("; ")
                )));
            }
        }
        let success = self
            .db
            .delete_ceph_cluster(name)
            .map_err(|e| Status::internal(e.to_string()))?;
        Ok(Response::new(controller_proto::DeleteCephClusterResponse {
            success,
        }))
    }

    async fn create_shared_filesystem(
        &self,
        request: Request<controller_proto::CreateSharedFilesystemRequest>,
    ) -> Result<Response<controller_proto::CreateSharedFilesystemResponse>, Status> {
        self.require_operator(&request, OperatorRole::ClusterAdmin)?;
        let actor = Self::audit_actor(&request);
        let incoming = request
            .into_inner()
            .shared_filesystem
            .ok_or_else(|| Status::invalid_argument("shared_filesystem is required"))?;
        let name = validate_network_name(&incoming.name)?;
        let spec = incoming
            .spec
            .ok_or_else(|| Status::invalid_argument("shared_filesystem.spec is required"))?;
        shared_filesystem_spec::validate_spec(&spec).map_err(Status::invalid_argument)?;
        if self
            .db
            .get_ceph_cluster(spec.ceph_cluster.trim())
            .map_err(|e| Status::internal(e.to_string()))?
            .is_none()
        {
            return Err(Status::failed_precondition(format!(
                "cephCluster '{}' is not registered",
                spec.ceph_cluster.trim()
            )));
        }
        let spec_json = shared_filesystem_spec::spec_to_json(&spec)
            .map_err(|e| Status::internal(format!("encode spec: {e}")))?;
        let existing = self
            .db
            .get_shared_filesystem(&name)
            .map_err(|e| Status::internal(e.to_string()))?;
        let (action, generation) = match existing.as_ref() {
            Some(e) if e.spec_json == spec_json => (
                controller_proto::ApplyAction::Unchanged as i32,
                e.generation,
            ),
            Some(e) => (
                controller_proto::ApplyAction::Updated as i32,
                e.generation.saturating_add(1),
            ),
            None => (controller_proto::ApplyAction::Created as i32, 1),
        };
        let row = if action == controller_proto::ApplyAction::Unchanged as i32 {
            existing.expect("unchanged requires existing")
        } else {
            let row = SharedFilesystemRow {
                name: name.clone(),
                generation,
                spec_json,
                created_at: existing
                    .as_ref()
                    .map(|e| e.created_at.clone())
                    .unwrap_or_default(),
                updated_at: String::new(),
            };
            self.db
                .upsert_shared_filesystem(&row)
                .map_err(|e| Status::internal(e.to_string()))?;
            self.db
                .upsert_shared_filesystem_status(&SharedFilesystemStatusRow {
                    name: name.clone(),
                    observed_generation: 0,
                    phase: "pending".into(),
                    health_message: String::new(),
                    last_transition_at: String::new(),
                })
                .map_err(|e| Status::internal(e.to_string()))?;
            self.db
                .get_shared_filesystem(&name)
                .map_err(|e| Status::internal(e.to_string()))?
                .unwrap()
        };
        self.record_audit(
            &actor,
            "CreateSharedFilesystem",
            &name,
            format!("action={action}"),
        );
        let status = self
            .db
            .get_shared_filesystem_status(&name)
            .map_err(|e| Status::internal(e.to_string()))?;
        Ok(Response::new(
            controller_proto::CreateSharedFilesystemResponse {
                success: true,
                shared_filesystem: Some(shared_filesystem_to_proto(&row, status)?),
                action,
                changed_fields: vec![],
            },
        ))
    }

    async fn get_shared_filesystem(
        &self,
        request: Request<controller_proto::GetSharedFilesystemRequest>,
    ) -> Result<Response<controller_proto::GetSharedFilesystemResponse>, Status> {
        self.require_operator(&request, OperatorRole::ReadOnly)?;
        let name = request.into_inner().name.trim().to_string();
        if name.is_empty() {
            return Err(Status::invalid_argument("name is required"));
        }
        let row = self
            .db
            .get_shared_filesystem(&name)
            .map_err(|e| Status::internal(e.to_string()))?
            .ok_or_else(|| Status::not_found(format!("SharedFilesystem '{name}' not found")))?;
        let status = self
            .db
            .get_shared_filesystem_status(&name)
            .map_err(|e| Status::internal(e.to_string()))?;
        Ok(Response::new(
            controller_proto::GetSharedFilesystemResponse {
                shared_filesystem: Some(shared_filesystem_to_proto(&row, status)?),
            },
        ))
    }

    async fn list_shared_filesystems(
        &self,
        request: Request<controller_proto::ListSharedFilesystemsRequest>,
    ) -> Result<Response<controller_proto::ListSharedFilesystemsResponse>, Status> {
        self.require_operator(&request, OperatorRole::ReadOnly)?;
        let mut out = Vec::new();
        for row in self
            .db
            .list_shared_filesystems()
            .map_err(|e| Status::internal(e.to_string()))?
        {
            let status = self
                .db
                .get_shared_filesystem_status(&row.name)
                .map_err(|e| Status::internal(e.to_string()))?;
            out.push(shared_filesystem_to_proto(&row, status)?);
        }
        Ok(Response::new(
            controller_proto::ListSharedFilesystemsResponse {
                shared_filesystems: out,
            },
        ))
    }

    async fn delete_shared_filesystem(
        &self,
        request: Request<controller_proto::DeleteSharedFilesystemRequest>,
    ) -> Result<Response<controller_proto::DeleteSharedFilesystemResponse>, Status> {
        self.require_operator(&request, OperatorRole::ClusterAdmin)?;
        let actor = Self::audit_actor(&request);
        let name = validate_network_name(&request.into_inner().name)?;
        let success = self
            .db
            .delete_shared_filesystem(&name)
            .map_err(|e| Status::internal(e.to_string()))?;
        if success {
            self.record_audit(&actor, "DeleteSharedFilesystem", &name, "");
        }
        Ok(Response::new(
            controller_proto::DeleteSharedFilesystemResponse { success },
        ))
    }

    async fn plan_ingress(
        &self,
        request: Request<controller_proto::PlanIngressRequest>,
    ) -> Result<Response<controller_proto::PlanIngressResponse>, Status> {
        self.ingress_plan(request).await
    }

    async fn create_ingress_gateway(
        &self,
        request: Request<controller_proto::CreateIngressGatewayRequest>,
    ) -> Result<Response<controller_proto::CreateIngressGatewayResponse>, Status> {
        self.ingress_create_gateway(request).await
    }

    async fn get_ingress_gateway(
        &self,
        request: Request<controller_proto::GetIngressGatewayRequest>,
    ) -> Result<Response<controller_proto::GetIngressGatewayResponse>, Status> {
        self.ingress_get_gateway(request).await
    }

    async fn list_ingress_gateways(
        &self,
        request: Request<controller_proto::ListIngressGatewaysRequest>,
    ) -> Result<Response<controller_proto::ListIngressGatewaysResponse>, Status> {
        self.ingress_list_gateways(request).await
    }

    async fn delete_ingress_gateway(
        &self,
        request: Request<controller_proto::DeleteIngressGatewayRequest>,
    ) -> Result<Response<controller_proto::DeleteIngressGatewayResponse>, Status> {
        self.ingress_delete_gateway(request).await
    }

    async fn create_ingress_route(
        &self,
        request: Request<controller_proto::CreateIngressRouteRequest>,
    ) -> Result<Response<controller_proto::CreateIngressRouteResponse>, Status> {
        self.ingress_create_route(request).await
    }

    async fn get_ingress_route(
        &self,
        request: Request<controller_proto::GetIngressRouteRequest>,
    ) -> Result<Response<controller_proto::GetIngressRouteResponse>, Status> {
        self.ingress_get_route(request).await
    }

    async fn list_ingress_routes(
        &self,
        request: Request<controller_proto::ListIngressRoutesRequest>,
    ) -> Result<Response<controller_proto::ListIngressRoutesResponse>, Status> {
        self.ingress_list_routes(request).await
    }

    async fn delete_ingress_route(
        &self,
        request: Request<controller_proto::DeleteIngressRouteRequest>,
    ) -> Result<Response<controller_proto::DeleteIngressRouteResponse>, Status> {
        self.ingress_delete_route(request).await
    }

    async fn set_ingress_route_waf_mode(
        &self,
        request: Request<controller_proto::SetIngressRouteWafModeRequest>,
    ) -> Result<Response<controller_proto::SetIngressRouteWafModeResponse>, Status> {
        self.ingress_set_route_waf_mode(request).await
    }

    async fn create_waf_policy(
        &self,
        request: Request<controller_proto::CreateWafPolicyRequest>,
    ) -> Result<Response<controller_proto::CreateWafPolicyResponse>, Status> {
        self.ingress_create_waf_policy(request).await
    }

    async fn get_waf_policy(
        &self,
        request: Request<controller_proto::GetWafPolicyRequest>,
    ) -> Result<Response<controller_proto::GetWafPolicyResponse>, Status> {
        self.ingress_get_waf_policy(request).await
    }

    async fn list_waf_policies(
        &self,
        request: Request<controller_proto::ListWafPoliciesRequest>,
    ) -> Result<Response<controller_proto::ListWafPoliciesResponse>, Status> {
        self.ingress_list_waf_policies(request).await
    }

    async fn delete_waf_policy(
        &self,
        request: Request<controller_proto::DeleteWafPolicyRequest>,
    ) -> Result<Response<controller_proto::DeleteWafPolicyResponse>, Status> {
        self.ingress_delete_waf_policy(request).await
    }

    async fn list_waf_events(
        &self,
        request: Request<controller_proto::ListWafEventsRequest>,
    ) -> Result<Response<controller_proto::ListWafEventsResponse>, Status> {
        self.ingress_list_waf_events(request).await
    }

    async fn report_waf_events(
        &self,
        request: Request<controller_proto::ReportWafEventsRequest>,
    ) -> Result<Response<controller_proto::ReportWafEventsResponse>, Status> {
        self.ingress_report_waf_events(request).await
    }

    async fn set_ingress_secret(
        &self,
        request: Request<controller_proto::SetIngressSecretRequest>,
    ) -> Result<Response<controller_proto::SetIngressSecretResponse>, Status> {
        self.ingress_set_secret(request).await
    }

    async fn create_object_store(
        &self,
        request: Request<controller_proto::CreateObjectStoreRequest>,
    ) -> Result<Response<controller_proto::CreateObjectStoreResponse>, Status> {
        self.require_operator(&request, OperatorRole::ClusterAdmin)?;
        let actor = Self::audit_actor(&request);
        let incoming = request
            .into_inner()
            .object_store
            .ok_or_else(|| Status::invalid_argument("object_store is required"))?;
        let name = validate_network_name(&incoming.name)?;
        let spec = incoming
            .spec
            .ok_or_else(|| Status::invalid_argument("object_store.spec is required"))?;
        object_store_spec::validate_spec(&spec).map_err(Status::invalid_argument)?;
        if self
            .db
            .get_ceph_cluster(spec.ceph_cluster.trim())
            .map_err(|e| Status::internal(e.to_string()))?
            .is_none()
        {
            return Err(Status::failed_precondition(format!(
                "cephCluster '{}' is not registered",
                spec.ceph_cluster.trim()
            )));
        }
        for member in &spec.members {
            if self
                .db
                .get_node(member.trim())
                .map_err(|e| Status::internal(e.to_string()))?
                .is_none()
            {
                return Err(Status::not_found(format!(
                    "member node '{}' is not registered",
                    member.trim()
                )));
            }
        }
        let spec_json = object_store_spec::spec_to_json(&spec)
            .map_err(|e| Status::internal(format!("encode spec: {e}")))?;
        let existing = self
            .db
            .get_object_store(&name)
            .map_err(|e| Status::internal(e.to_string()))?;
        let (action, generation) = match existing.as_ref() {
            Some(e) if e.spec_json == spec_json => (
                controller_proto::ApplyAction::Unchanged as i32,
                e.generation,
            ),
            Some(e) => (
                controller_proto::ApplyAction::Updated as i32,
                e.generation.saturating_add(1),
            ),
            None => (controller_proto::ApplyAction::Created as i32, 1),
        };
        let row = if action == controller_proto::ApplyAction::Unchanged as i32 {
            existing.expect("unchanged requires existing")
        } else {
            let row = ObjectStoreRow {
                name: name.clone(),
                generation,
                spec_json,
                created_at: existing
                    .as_ref()
                    .map(|e| e.created_at.clone())
                    .unwrap_or_default(),
                updated_at: String::new(),
            };
            self.db
                .upsert_object_store(&row)
                .map_err(|e| Status::internal(e.to_string()))?;
            self.db
                .upsert_object_store_status(&ObjectStoreStatusRow {
                    name: name.clone(),
                    observed_generation: 0,
                    phase: "pending".into(),
                    health_message: String::new(),
                    last_transition_at: String::new(),
                })
                .map_err(|e| Status::internal(e.to_string()))?;
            self.db
                .get_object_store(&name)
                .map_err(|e| Status::internal(e.to_string()))?
                .unwrap()
        };
        self.record_audit(
            &actor,
            "CreateObjectStore",
            &name,
            format!("action={action}"),
        );
        let status = self
            .db
            .get_object_store_status(&name)
            .map_err(|e| Status::internal(e.to_string()))?;
        Ok(Response::new(controller_proto::CreateObjectStoreResponse {
            success: true,
            object_store: Some(object_store_to_proto(&row, status)?),
            action,
            changed_fields: vec![],
        }))
    }

    async fn get_object_store(
        &self,
        request: Request<controller_proto::GetObjectStoreRequest>,
    ) -> Result<Response<controller_proto::GetObjectStoreResponse>, Status> {
        self.require_operator(&request, OperatorRole::ReadOnly)?;
        let name = request.into_inner().name.trim().to_string();
        if name.is_empty() {
            return Err(Status::invalid_argument("name is required"));
        }
        let row = self
            .db
            .get_object_store(&name)
            .map_err(|e| Status::internal(e.to_string()))?
            .ok_or_else(|| Status::not_found(format!("ObjectStore '{name}' not found")))?;
        let status = self
            .db
            .get_object_store_status(&name)
            .map_err(|e| Status::internal(e.to_string()))?;
        Ok(Response::new(controller_proto::GetObjectStoreResponse {
            object_store: Some(object_store_to_proto(&row, status)?),
        }))
    }

    async fn list_object_stores(
        &self,
        request: Request<controller_proto::ListObjectStoresRequest>,
    ) -> Result<Response<controller_proto::ListObjectStoresResponse>, Status> {
        self.require_operator(&request, OperatorRole::ReadOnly)?;
        let mut out = Vec::new();
        for row in self
            .db
            .list_object_stores()
            .map_err(|e| Status::internal(e.to_string()))?
        {
            let status = self
                .db
                .get_object_store_status(&row.name)
                .map_err(|e| Status::internal(e.to_string()))?;
            out.push(object_store_to_proto(&row, status)?);
        }
        Ok(Response::new(controller_proto::ListObjectStoresResponse {
            object_stores: out,
        }))
    }

    async fn delete_object_store(
        &self,
        request: Request<controller_proto::DeleteObjectStoreRequest>,
    ) -> Result<Response<controller_proto::DeleteObjectStoreResponse>, Status> {
        self.require_operator(&request, OperatorRole::ClusterAdmin)?;
        let actor = Self::audit_actor(&request);
        let name = validate_network_name(&request.into_inner().name)?;
        let users = self
            .db
            .list_object_users_for_store(&name)
            .map_err(|e| Status::internal(e.to_string()))?;
        if !users.is_empty() {
            return Err(Status::failed_precondition(format!(
                "ObjectStore '{name}' still has {} user(s); delete them first",
                users.len()
            )));
        }
        let success = self
            .db
            .delete_object_store(&name)
            .map_err(|e| Status::internal(e.to_string()))?;
        if success {
            self.record_audit(&actor, "DeleteObjectStore", &name, "");
        }
        Ok(Response::new(controller_proto::DeleteObjectStoreResponse {
            success,
        }))
    }

    async fn create_object_user(
        &self,
        request: Request<controller_proto::CreateObjectUserRequest>,
    ) -> Result<Response<controller_proto::CreateObjectUserResponse>, Status> {
        self.require_operator(&request, OperatorRole::ClusterAdmin)?;
        let actor = Self::audit_actor(&request);
        let incoming = request
            .into_inner()
            .object_user
            .ok_or_else(|| Status::invalid_argument("object_user is required"))?;
        let name = validate_network_name(&incoming.name)?;
        let spec = incoming
            .spec
            .ok_or_else(|| Status::invalid_argument("object_user.spec is required"))?;
        let store = spec.store.trim();
        if store.is_empty() {
            return Err(Status::invalid_argument(
                "object_user.spec.store is required",
            ));
        }
        if self
            .db
            .get_object_store(store)
            .map_err(|e| Status::internal(e.to_string()))?
            .is_none()
        {
            return Err(Status::not_found(format!(
                "ObjectStore '{store}' not found"
            )));
        }
        if self
            .db
            .get_object_user(&name, store)
            .map_err(|e| Status::internal(e.to_string()))?
            .is_some()
        {
            return Err(Status::already_exists(format!(
                "ObjectUser '{name}' already exists on store '{store}'"
            )));
        }
        let (access_key, secret) = new_object_user_credentials();
        let row = ObjectUserRow {
            name: name.clone(),
            store_name: store.to_string(),
            access_key,
            secret: secret.clone(),
            created_at: String::new(),
        };
        self.db
            .insert_object_user(&row)
            .map_err(|e| Status::internal(e.to_string()))?;
        self.record_audit(
            &actor,
            "CreateObjectUser",
            &format!("{store}/{name}"),
            "secret issued once",
        );
        let stored = self
            .db
            .get_object_user(&name, store)
            .map_err(|e| Status::internal(e.to_string()))?
            .unwrap();
        Ok(Response::new(controller_proto::CreateObjectUserResponse {
            success: true,
            object_user: Some(object_user_to_proto(&stored)),
            secret,
        }))
    }

    async fn delete_object_user(
        &self,
        request: Request<controller_proto::DeleteObjectUserRequest>,
    ) -> Result<Response<controller_proto::DeleteObjectUserResponse>, Status> {
        self.require_operator(&request, OperatorRole::ClusterAdmin)?;
        let actor = Self::audit_actor(&request);
        let req = request.into_inner();
        let name = validate_network_name(&req.name)?;
        let store = req.store.trim();
        if store.is_empty() {
            return Err(Status::invalid_argument("store is required"));
        }
        let success = self
            .db
            .delete_object_user(&name, store)
            .map_err(|e| Status::internal(e.to_string()))?;
        if success {
            self.record_audit(&actor, "DeleteObjectUser", &format!("{store}/{name}"), "");
        }
        Ok(Response::new(controller_proto::DeleteObjectUserResponse {
            success,
        }))
    }

    async fn create_cluster_update(
        &self,
        request: Request<controller_proto::CreateClusterUpdateRequest>,
    ) -> Result<Response<controller_proto::CreateClusterUpdateResponse>, Status> {
        self.require_operator(&request, OperatorRole::ClusterAdmin)?;
        let actor = Self::audit_actor(&request);
        let req = request.into_inner();
        let spec = req
            .spec
            .ok_or_else(|| Status::invalid_argument("spec is required"))?;
        cluster_update_spec::validate_spec(&spec).map_err(Status::invalid_argument)?;
        let target_ids = cluster_update_spec::resolve_target_node_ids(&self.db, &spec)
            .map_err(Status::invalid_argument)?;
        if target_ids.is_empty() {
            return Err(Status::invalid_argument("selector matched no nodes"));
        }
        let spec_json = cluster_update_spec::spec_to_json(&spec)
            .map_err(|e| Status::internal(format!("encode spec: {e}")))?;
        let name = spec.name.trim().to_string();
        let existing = self
            .db
            .get_cluster_update(&name)
            .map_err(|e| Status::internal(e.to_string()))?;
        let target = spec
            .target
            .as_ref()
            .ok_or_else(|| Status::invalid_argument("spec.target is required"))?;
        let target_version = target.version.trim().to_string();
        let flake_ref = target.flake_ref.trim().to_string();
        let flake_rev = target.flake_rev.trim().to_string();

        let (action, generation, changed_fields) = if let Some(ex) = existing.as_ref() {
            if ex.spec_json == spec_json {
                (
                    controller_proto::ApplyAction::Unchanged as i32,
                    ex.generation,
                    Vec::<String>::new(),
                )
            } else {
                (
                    controller_proto::ApplyAction::Updated as i32,
                    ex.generation.saturating_add(1),
                    vec!["spec".to_string()],
                )
            }
        } else {
            (
                controller_proto::ApplyAction::Created as i32,
                1i64,
                Vec::<String>::new(),
            )
        };

        if action == controller_proto::ApplyAction::Unchanged as i32 {
            let ex = existing.expect("unchanged implies existing");
            let parsed = cluster_update_spec::spec_from_json(&ex.spec_json)
                .map_err(|e| Status::internal(format!("decode spec: {e}")))?;
            return Ok(Response::new(
                controller_proto::CreateClusterUpdateResponse {
                    success: true,
                    cluster_update: Some(cluster_update_row_to_proto(&ex, parsed)),
                    action,
                    changed_fields,
                },
            ));
        }

        let manual = cluster_update_spec::requires_manual_approval(&spec);
        let (approval_status, phase) = if manual {
            ("awaiting", "pending")
        } else {
            ("approved", "ready")
        };
        let created_at = existing
            .as_ref()
            .map(|e| e.created_at.clone())
            .unwrap_or_default();

        let row = ClusterUpdateRow {
            name: name.clone(),
            generation,
            target_version,
            flake_ref,
            flake_rev,
            spec_json: spec_json.clone(),
            phase: phase.to_string(),
            approval_status: approval_status.to_string(),
            created_at,
            updated_at: String::new(),
        };
        self.db
            .upsert_cluster_update(&row)
            .map_err(|e| Status::internal(e.to_string()))?;

        let mut node_rows: Vec<ClusterUpdateNodeRow> = Vec::new();
        for nid in &target_ids {
            node_rows.push(ClusterUpdateNodeRow {
                update_name: name.clone(),
                node_id: nid.clone(),
                observed_generation: generation,
                phase: "pending".to_string(),
                current_version: String::new(),
                target_version: target.version.trim().to_string(),
                prepared_closure: String::new(),
                current_generation: String::new(),
                target_generation: String::new(),
                requires_reboot: false,
                last_error: String::new(),
                last_transition_at: String::new(),
            });
        }
        self.db
            .replace_cluster_update_nodes(&name, &node_rows)
            .map_err(|e| Status::internal(e.to_string()))?;

        let stored = self
            .db
            .get_cluster_update(&name)
            .map_err(|e| Status::internal(e.to_string()))?
            .expect("just inserted");
        let parsed_spec = cluster_update_spec::spec_from_json(&stored.spec_json)
            .map_err(|e| Status::internal(format!("decode spec: {e}")))?;

        let _ = self.log_replication_event_required(
            &actor,
            Some("CreateClusterUpdate"),
            EVT_CLUSTER_UPDATE_CREATE,
            &name,
            serde_json::json!({ "generation": generation }),
        );

        Ok(Response::new(
            controller_proto::CreateClusterUpdateResponse {
                success: true,
                cluster_update: Some(cluster_update_row_to_proto(&stored, parsed_spec)),
                action,
                changed_fields,
            },
        ))
    }

    async fn get_cluster_update(
        &self,
        request: Request<controller_proto::GetClusterUpdateRequest>,
    ) -> Result<Response<controller_proto::GetClusterUpdateResponse>, Status> {
        self.require_operator(&request, OperatorRole::ReadOnly)?;
        let req = request.into_inner();
        if req.name.trim().is_empty() {
            return Err(Status::invalid_argument("name is required"));
        }
        let row = self
            .db
            .get_cluster_update(req.name.trim())
            .map_err(|e| Status::internal(e.to_string()))?
            .ok_or_else(|| Status::not_found(format!("cluster update '{}' not found", req.name)))?;
        let parsed = cluster_update_spec::spec_from_json(&row.spec_json)
            .map_err(|e| Status::internal(format!("decode spec: {e}")))?;
        let statuses = self
            .db
            .list_cluster_update_nodes(req.name.trim())
            .map_err(|e| Status::internal(e.to_string()))?;
        let node_statuses: Vec<controller_proto::NodeUpdateStatus> =
            statuses.iter().map(node_update_row_to_proto).collect();
        Ok(Response::new(controller_proto::GetClusterUpdateResponse {
            cluster_update: Some(cluster_update_row_to_proto(&row, parsed)),
            node_statuses,
        }))
    }

    async fn list_cluster_updates(
        &self,
        request: Request<controller_proto::ListClusterUpdatesRequest>,
    ) -> Result<Response<controller_proto::ListClusterUpdatesResponse>, Status> {
        self.require_operator(&request, OperatorRole::ReadOnly)?;
        let _ = request.into_inner();
        let rows = self
            .db
            .list_cluster_updates()
            .map_err(|e| Status::internal(e.to_string()))?;
        let mut cluster_updates = Vec::with_capacity(rows.len());
        for row in rows {
            let parsed = cluster_update_spec::spec_from_json(&row.spec_json)
                .map_err(|e| Status::internal(format!("decode spec for {}: {e}", row.name)))?;
            cluster_updates.push(cluster_update_row_to_proto(&row, parsed));
        }
        Ok(Response::new(
            controller_proto::ListClusterUpdatesResponse { cluster_updates },
        ))
    }

    async fn plan_cluster_update(
        &self,
        request: Request<controller_proto::PlanClusterUpdateRequest>,
    ) -> Result<Response<controller_proto::PlanClusterUpdateResponse>, Status> {
        self.require_operator(&request, OperatorRole::ReadOnly)?;
        let req = request.into_inner();
        let spec = req
            .spec
            .ok_or_else(|| Status::invalid_argument("spec is required"))?;
        cluster_update_spec::validate_spec(&spec).map_err(Status::invalid_argument)?;
        let target_ids = cluster_update_spec::resolve_target_node_ids(&self.db, &spec)
            .map_err(Status::invalid_argument)?;
        let mut issues = Vec::new();
        for nid in &target_ids {
            if self
                .db
                .get_node(nid)
                .map_err(|e| Status::internal(e.to_string()))?
                .is_none()
            {
                issues.push(controller_proto::PlanIssue {
                    node_id: nid.clone(),
                    reason: "node not registered".into(),
                });
            } else if let Ok(Some(host)) = self.db.get_node_host_status(nid) {
                if spec.schema_version > 0 && host.schema_version > spec.schema_version {
                    issues.push(controller_proto::PlanIssue {
                        node_id: nid.clone(),
                        reason: "node schema is newer than this update".into(),
                    });
                }
                if spec.min_compatible_reader > 0
                    && host.schema_version > 0
                    && host.schema_version < spec.min_compatible_reader
                {
                    issues.push(controller_proto::PlanIssue {
                        node_id: nid.clone(),
                        reason: "node schema is older than minCompatibleReader".into(),
                    });
                }
            }
        }
        let viable = !target_ids.is_empty() && issues.is_empty();
        let slurm_members = target_ids
            .iter()
            .any(|nid| self.db.get_slurm_cluster_node(nid).ok().flatten().is_some());
        let detail = if slurm_members {
            "Members of one Slurm cluster must be in the same cluster-update run because Slurm requires dbd >= ctld >= slurmd. Compute nodes are drained before their rebuild."
        } else {
            "MVP: assumes flake rollout may restart kcore services"
        };
        Ok(Response::new(controller_proto::PlanClusterUpdateResponse {
            viable,
            target_node_ids: target_ids,
            issues,
            likely_requires_reboot: true,
            detail: detail.into(),
        }))
    }

    async fn approve_cluster_update(
        &self,
        request: Request<controller_proto::ApproveClusterUpdateRequest>,
    ) -> Result<Response<controller_proto::ApproveClusterUpdateResponse>, Status> {
        self.require_operator(&request, OperatorRole::ClusterAdmin)?;
        let actor = Self::audit_actor(&request);
        let req = request.into_inner();
        if req.name.trim().is_empty() {
            return Err(Status::invalid_argument("name is required"));
        }
        let name = req.name.trim().to_string();
        let existing = self
            .db
            .get_cluster_update(&name)
            .map_err(|e| Status::internal(e.to_string()))?
            .ok_or_else(|| Status::not_found(format!("cluster update '{}' not found", req.name)))?;
        let mut row = existing.clone();
        row.approval_status = "approved".into();
        if row.phase == "pending" {
            row.phase = "ready".into();
        }
        self.db
            .upsert_cluster_update(&row)
            .map_err(|e| Status::internal(e.to_string()))?;
        let stored = self
            .db
            .get_cluster_update(&name)
            .map_err(|e| Status::internal(e.to_string()))?
            .expect("row exists");
        let parsed = cluster_update_spec::spec_from_json(&stored.spec_json)
            .map_err(|e| Status::internal(format!("decode spec: {e}")))?;
        let _ = self.log_replication_event_required(
            &actor,
            Some("ApproveClusterUpdate"),
            EVT_CLUSTER_UPDATE_APPROVE,
            &name,
            serde_json::json!({}),
        );
        Ok(Response::new(
            controller_proto::ApproveClusterUpdateResponse {
                success: true,
                cluster_update: Some(cluster_update_row_to_proto(&stored, parsed)),
            },
        ))
    }

    async fn cancel_cluster_update(
        &self,
        request: Request<controller_proto::CancelClusterUpdateRequest>,
    ) -> Result<Response<controller_proto::CancelClusterUpdateResponse>, Status> {
        self.require_operator(&request, OperatorRole::ClusterAdmin)?;
        let actor = Self::audit_actor(&request);
        let req = request.into_inner();
        if req.name.trim().is_empty() {
            return Err(Status::invalid_argument("name is required"));
        }
        let name = req.name.trim().to_string();
        let existing = self
            .db
            .get_cluster_update(&name)
            .map_err(|e| Status::internal(e.to_string()))?
            .ok_or_else(|| Status::not_found(format!("cluster update '{}' not found", req.name)))?;
        let mut row = existing.clone();
        row.phase = "cancelled".into();
        self.db
            .upsert_cluster_update(&row)
            .map_err(|e| Status::internal(e.to_string()))?;
        let nodes = self
            .db
            .list_cluster_update_nodes(&name)
            .map_err(|e| Status::internal(e.to_string()))?;
        for n in nodes {
            if n.phase != "succeeded" {
                self.db
                    .upsert_cluster_update_node(&ClusterUpdateNodeRow {
                        phase: "cancelled".into(),
                        ..n
                    })
                    .map_err(|e| Status::internal(e.to_string()))?;
            }
        }
        let stored = self
            .db
            .get_cluster_update(&name)
            .map_err(|e| Status::internal(e.to_string()))?
            .expect("row exists");
        let parsed = cluster_update_spec::spec_from_json(&stored.spec_json)
            .map_err(|e| Status::internal(format!("decode spec: {e}")))?;
        let _ = self.log_replication_event_required(
            &actor,
            Some("CancelClusterUpdate"),
            EVT_CLUSTER_UPDATE_CANCEL,
            &name,
            serde_json::json!({}),
        );
        Ok(Response::new(
            controller_proto::CancelClusterUpdateResponse {
                success: true,
                cluster_update: Some(cluster_update_row_to_proto(&stored, parsed)),
            },
        ))
    }

    async fn rollback_cluster_update(
        &self,
        request: Request<controller_proto::RollbackClusterUpdateRequest>,
    ) -> Result<Response<controller_proto::RollbackClusterUpdateResponse>, Status> {
        self.require_operator(&request, OperatorRole::ClusterAdmin)?;
        let actor = Self::audit_actor(&request);
        let req = request.into_inner();
        if req.name.trim().is_empty() {
            return Err(Status::invalid_argument("name is required"));
        }
        let name = req.name.trim().to_string();
        let existing = self
            .db
            .get_cluster_update(&name)
            .map_err(|e| Status::internal(e.to_string()))?
            .ok_or_else(|| Status::not_found(format!("cluster update '{}' not found", req.name)))?;
        let mut row = existing.clone();
        row.phase = "rolling_back".into();
        self.db
            .upsert_cluster_update(&row)
            .map_err(|e| Status::internal(e.to_string()))?;

        // Mark non-terminal nodes as `rolling_back` so the reconciler picks
        // them up. Nodes that already activated are the ones we actually
        // need to roll back; nodes still pending/prepared also get marked
        // so the reconciler can quickly cancel their staging directories.
        let nodes = self
            .db
            .list_cluster_update_nodes(&name)
            .map_err(|e| Status::internal(e.to_string()))?;
        for n in nodes {
            if n.phase == "succeeded"
                || n.phase == "prepared"
                || n.phase == "pending"
                || n.phase == "reboot_required"
                || n.phase == "bootstrapping"
            {
                self.db
                    .upsert_cluster_update_node(&ClusterUpdateNodeRow {
                        phase: "rolling_back".into(),
                        ..n
                    })
                    .map_err(|e| Status::internal(e.to_string()))?;
            }
        }

        let stored = self
            .db
            .get_cluster_update(&name)
            .map_err(|e| Status::internal(e.to_string()))?
            .expect("row exists");
        let parsed = cluster_update_spec::spec_from_json(&stored.spec_json)
            .map_err(|e| Status::internal(format!("decode spec: {e}")))?;
        let _ = self.log_replication_event_required(
            &actor,
            Some("RollbackClusterUpdate"),
            EVT_CLUSTER_UPDATE_ROLLBACK,
            &name,
            serde_json::json!({}),
        );
        Ok(Response::new(
            controller_proto::RollbackClusterUpdateResponse {
                success: true,
                cluster_update: Some(cluster_update_row_to_proto(&stored, parsed)),
            },
        ))
    }

    async fn reboot_cluster_update_node(
        &self,
        request: Request<controller_proto::RebootClusterUpdateNodeRequest>,
    ) -> Result<Response<controller_proto::RebootClusterUpdateNodeResponse>, Status> {
        self.require_operator(&request, OperatorRole::ClusterAdmin)?;
        let req = request.into_inner();
        if req.name.trim().is_empty() || req.node_id.trim().is_empty() {
            return Err(Status::invalid_argument("name and node_id are required"));
        }
        let update = self
            .db
            .get_cluster_update(req.name.trim())
            .map_err(|e| Status::internal(e.to_string()))?
            .ok_or_else(|| Status::not_found("cluster update not found"))?;
        let spec = cluster_update_spec::spec_from_json(&update.spec_json)
            .map_err(|e| Status::internal(e.to_string()))?;
        let node_row = self
            .db
            .list_cluster_update_nodes(req.name.trim())
            .map_err(|e| Status::internal(e.to_string()))?
            .into_iter()
            .find(|row| row.node_id == req.node_id)
            .ok_or_else(|| Status::not_found("node is not part of this update"))?;
        if node_row.phase != "reboot_required" {
            return Err(Status::failed_precondition(
                "node has no staged reboot; apply finishes userspace switches without one",
            ));
        }
        let vms = self
            .db
            .list_vms_for_node(&req.node_id)
            .map_err(|e| Status::internal(e.to_string()))?;
        if !vms.is_empty() {
            if !spec.drain_vms {
                return Err(Status::failed_precondition(
                    "guests are still on this node; migrate them or set drainVms before reboot",
                ));
            }
            let outcome = self
                .drain_node_vms(&req.node_id, "", NodeDrainOptions::default())
                .await?;
            if !outcome.errors.is_empty() {
                return Err(Status::failed_precondition(format!(
                    "drain failed: {}",
                    outcome.errors.join("; ")
                )));
            }
        }
        let node = self
            .db
            .get_node(&req.node_id)
            .map_err(|e| Status::internal(e.to_string()))?
            .ok_or_else(|| Status::not_found("node not registered"))?;
        if self.clients.get_admin(&node.address).is_none() {
            self.clients
                .connect(&node.address)
                .await
                .map_err(|e| Status::unavailable(e.to_string()))?;
        }
        let mut admin = self
            .clients
            .get_admin(&node.address)
            .ok_or_else(|| Status::unavailable("node admin client unavailable"))?;
        let resp = admin
            .reboot_staged_system_update(Request::new(
                crate::node_proto::RebootStagedSystemUpdateRequest {
                    update_name: req.name,
                },
            ))
            .await
            .map_err(|e| Status::unavailable(format!("reboot: {e}")))?
            .into_inner();
        Ok(Response::new(
            controller_proto::RebootClusterUpdateNodeResponse {
                success: resp.success,
                message: resp.message,
            },
        ))
    }

    async fn create_operator(
        &self,
        request: Request<controller_proto::CreateOperatorRequest>,
    ) -> Result<Response<controller_proto::CreateOperatorResponse>, Status> {
        self.require_operator(&request, OperatorRole::ClusterAdmin)?;
        let actor = Self::audit_actor(&request);
        let name = request.into_inner().name.trim().to_string();
        auth::validate_operator_name(&name)?;
        if self
            .db
            .get_operator_row(&name)
            .map_err(internal_db)?
            .is_some()
        {
            return Err(Status::already_exists(format!(
                "operator '{name}' already exists"
            )));
        }
        self.db.create_operator(&name).map_err(internal_db)?;
        self.log_replication_event(
            &actor,
            Some("CreateOperator"),
            EVT_OPERATOR_UPSERT,
            &format!("operator/{name}"),
            serde_json::json!({ "name": name }),
        );
        let row = self
            .db
            .get_operator_row(&name)
            .map_err(internal_db)?
            .expect("just inserted");
        let operator = operator_row_to_proto(&self.db, &row).map_err(internal_db)?;
        Ok(Response::new(controller_proto::CreateOperatorResponse {
            operator: Some(operator),
        }))
    }

    async fn delete_operator(
        &self,
        request: Request<controller_proto::DeleteOperatorRequest>,
    ) -> Result<Response<controller_proto::DeleteOperatorResponse>, Status> {
        self.require_operator(&request, OperatorRole::ClusterAdmin)?;
        let actor = Self::audit_actor(&request);
        let name = request.into_inner().name.trim().to_string();
        if name.is_empty() {
            return Err(Status::invalid_argument("name is required"));
        }
        let ok = self.db.delete_operator(&name).map_err(internal_db)?;
        if ok {
            self.log_replication_event(
                &actor,
                Some("DeleteOperator"),
                EVT_OPERATOR_DELETE,
                &format!("operator/{name}"),
                serde_json::json!({ "name": name }),
            );
        }
        Ok(Response::new(controller_proto::DeleteOperatorResponse {
            success: ok,
        }))
    }

    async fn list_operators(
        &self,
        request: Request<controller_proto::ListOperatorsRequest>,
    ) -> Result<Response<controller_proto::ListOperatorsResponse>, Status> {
        self.require_operator(&request, OperatorRole::ClusterAdmin)?;
        let _ = request.into_inner();
        let rows = self.db.list_operator_rows().map_err(internal_db)?;
        let mut operators = Vec::with_capacity(rows.len());
        for row in rows {
            operators.push(operator_row_to_proto(&self.db, &row).map_err(internal_db)?);
        }
        Ok(Response::new(controller_proto::ListOperatorsResponse {
            operators,
        }))
    }

    async fn get_operator(
        &self,
        request: Request<controller_proto::GetOperatorRequest>,
    ) -> Result<Response<controller_proto::GetOperatorResponse>, Status> {
        self.require_operator(&request, OperatorRole::ClusterAdmin)?;
        let name = request.into_inner().name.trim().to_string();
        if name.is_empty() {
            return Err(Status::invalid_argument("name is required"));
        }
        let row = self
            .db
            .get_operator_row(&name)
            .map_err(internal_db)?
            .ok_or_else(|| Status::not_found(format!("operator '{name}' not found")))?;
        let operator = operator_row_to_proto(&self.db, &row).map_err(internal_db)?;
        Ok(Response::new(controller_proto::GetOperatorResponse {
            operator: Some(operator),
        }))
    }

    async fn grant_operator_role(
        &self,
        request: Request<controller_proto::GrantOperatorRoleRequest>,
    ) -> Result<Response<controller_proto::GrantOperatorRoleResponse>, Status> {
        self.require_operator(&request, OperatorRole::ClusterAdmin)?;
        let actor = Self::audit_actor(&request);
        let req = request.into_inner();
        let op_name = req.operator_name.trim();
        if op_name.is_empty() {
            return Err(Status::invalid_argument("operator_name is required"));
        }
        let role = operator_role_kind_to_auth(req.role)?;
        if self
            .db
            .get_operator_row(op_name)
            .map_err(internal_db)?
            .is_none()
        {
            return Err(Status::not_found(format!("operator '{op_name}' not found")));
        }
        self.db
            .grant_operator_role_str(op_name, role.as_db_str())
            .map_err(internal_db)?;
        self.db
            .touch_operator_updated(op_name)
            .map_err(internal_db)?;
        self.log_replication_event(
            &actor,
            Some("GrantOperatorRole"),
            EVT_OPERATOR_ROLE_GRANT,
            &format!("operator-role/{op_name}/{}", role.as_db_str()),
            serde_json::json!({
                "operatorName": op_name,
                "role": role.as_db_str(),
            }),
        );
        let row = self
            .db
            .get_operator_row(op_name)
            .map_err(internal_db)?
            .expect("operator exists");
        let operator = operator_row_to_proto(&self.db, &row).map_err(internal_db)?;
        Ok(Response::new(controller_proto::GrantOperatorRoleResponse {
            operator: Some(operator),
        }))
    }

    async fn revoke_operator_role(
        &self,
        request: Request<controller_proto::RevokeOperatorRoleRequest>,
    ) -> Result<Response<controller_proto::RevokeOperatorRoleResponse>, Status> {
        self.require_operator(&request, OperatorRole::ClusterAdmin)?;
        let actor = Self::audit_actor(&request);
        let req = request.into_inner();
        let op_name = req.operator_name.trim();
        if op_name.is_empty() {
            return Err(Status::invalid_argument("operator_name is required"));
        }
        let role = operator_role_kind_to_auth(req.role)?;
        let _ = self
            .db
            .revoke_operator_role_str(op_name, role.as_db_str())
            .map_err(internal_db)?;
        self.db
            .touch_operator_updated(op_name)
            .map_err(internal_db)?;
        self.log_replication_event(
            &actor,
            Some("RevokeOperatorRole"),
            EVT_OPERATOR_ROLE_REVOKE,
            &format!("operator-role/{op_name}/{}", role.as_db_str()),
            serde_json::json!({
                "operatorName": op_name,
                "role": role.as_db_str(),
            }),
        );
        let row = self.db.get_operator_row(op_name).map_err(internal_db)?;
        let operator = match row {
            Some(r) => operator_row_to_proto(&self.db, &r).map_err(internal_db)?,
            None => controller_proto::Operator {
                name: op_name.to_string(),
                ..Default::default()
            },
        };
        Ok(Response::new(
            controller_proto::RevokeOperatorRoleResponse {
                operator: Some(operator),
            },
        ))
    }

    async fn issue_operator_cert(
        &self,
        request: Request<controller_proto::IssueOperatorCertRequest>,
    ) -> Result<Response<controller_proto::IssueOperatorCertResponse>, Status> {
        self.require_operator(&request, OperatorRole::ClusterAdmin)?;
        let actor = Self::audit_actor(&request);
        let op_name = request.into_inner().operator_name.trim().to_string();
        if op_name.is_empty() {
            return Err(Status::invalid_argument("operator_name is required"));
        }
        if self
            .db
            .get_operator_row(&op_name)
            .map_err(internal_db)?
            .is_none()
        {
            return Err(Status::not_found(format!("operator '{op_name}' not found")));
        }

        let sub_ca = self
            .sub_ca
            .lock()
            .map_err(|_| Status::internal("sub-CA lock poisoned"))?
            .clone();

        if !sub_ca.is_available() {
            return Err(Status::unavailable(
                "sub-CA is not configured on this controller; operator certificate issuance is unavailable",
            ));
        }

        let (chain_pem, key_pem) =
            signing::sign_operator_cert(&sub_ca.cert_pem, &sub_ca.key_pem, &op_name)
                .map_err(|e| Status::internal(format!("signing operator cert: {e}")))?;

        let serial = leaf_serial_hex(&chain_pem)?;
        self.record_issued_cert(&chain_pem, "");
        self.db
            .set_operator_cert_serial(&op_name, &serial)
            .map_err(internal_db)?;
        self.log_replication_event(
            &actor,
            Some("IssueOperatorCert"),
            EVT_OPERATOR_UPSERT,
            &format!("operator/{op_name}"),
            serde_json::json!({
                "name": op_name,
                "certSerial": serial,
            }),
        );

        info!(operator = %op_name, "issued operator client certificate via sub-CA");

        Ok(Response::new(controller_proto::IssueOperatorCertResponse {
            success: true,
            cert_pem: chain_pem,
            key_pem,
            message: format!("operator certificate issued for '{op_name}'"),
        }))
    }

    // ============================================================================
    // Kubernetes Cluster Management (Phase 3 implementation)
    // ============================================================================

    async fn plan_slurm_cluster(
        &self,
        request: Request<controller_proto::PlanSlurmClusterRequest>,
    ) -> Result<Response<controller_proto::PlanSlurmClusterResponse>, Status> {
        self.require_operator(&request, OperatorRole::ClusterAdmin)?;
        let req = request.into_inner();
        let name = validate_network_name(&req.name)?;
        let spec = slurm_spec_from_proto(
            req.spec
                .ok_or_else(|| Status::invalid_argument("spec is required"))?,
        );
        let plan = self.plan_slurm_cluster_spec(&name, &spec).await?;
        Ok(Response::new(plan))
    }

    async fn create_slurm_cluster(
        &self,
        request: Request<controller_proto::CreateSlurmClusterRequest>,
    ) -> Result<Response<controller_proto::CreateSlurmClusterResponse>, Status> {
        self.require_operator(&request, OperatorRole::ClusterAdmin)?;
        let actor = Self::audit_actor(&request);
        let req = request.into_inner();
        let name = validate_network_name(&req.name)?;
        let spec = slurm_spec_from_proto(
            req.spec
                .ok_or_else(|| Status::invalid_argument("spec is required"))?,
        );
        crate::slurm_cluster_spec::validate_spec(&spec).map_err(Status::invalid_argument)?;

        let existing = self
            .db
            .get_slurm_cluster(&name)
            .map_err(|e| Status::internal(format!("reading slurm cluster: {e}")))?;
        let existing_spec = existing
            .as_ref()
            .and_then(|row| crate::slurm_cluster_spec::spec_from_json(&row.spec_json).ok());
        let (action, changed_fields) =
            crate::slurm_cluster_spec::plan_action(existing_spec.as_ref(), &spec);
        if action == "replace" {
            let field = existing_spec
                .as_ref()
                .and_then(|old| crate::slurm_cluster_spec::immutable_change(old, &spec))
                .unwrap_or_else(|| "spec".into());
            return Err(Status::failed_precondition(format!(
                "cannot change immutable field {field} on SlurmCluster '{name}'; delete it and create it again"
            )));
        }
        if action == "unchanged" {
            let row = existing.expect("unchanged requires existing");
            let nodes = self
                .db
                .list_slurm_cluster_nodes(&name)
                .map_err(|e| Status::internal(format!("listing slurm nodes: {e}")))?;
            return Ok(Response::new(
                controller_proto::CreateSlurmClusterResponse {
                    success: true,
                    message: format!("SlurmCluster '{name}' unchanged"),
                    cluster: Some(slurm_cluster_to_proto(&row, &nodes)?),
                    action: controller_proto::ApplyAction::Unchanged as i32,
                    changed_fields: Vec::new(),
                },
            ));
        }

        let plan = self.plan_slurm_cluster_spec(&name, &spec).await?;
        if !plan.blocking_issues.is_empty() {
            return Err(Status::failed_precondition(format!(
                "SlurmCluster preflight failed:\n{}",
                plan.blocking_issues.join("\n")
            )));
        }

        let mut munge_key = existing
            .as_ref()
            .map(|row| row.munge_key.clone())
            .unwrap_or_default();
        if munge_key.is_empty() {
            munge_key = slurm_random_bytes(1024)?;
        }
        let mut dbd_password = existing
            .as_ref()
            .map(|row| row.dbd_password.clone())
            .unwrap_or_default();
        if spec.accounting.enabled && dbd_password.is_empty() {
            dbd_password = slurm_hex(&slurm_random_bytes(32)?);
        }
        let generation = match &existing {
            Some(row) => row.generation.saturating_add(1),
            None => 1,
        };
        let spec_json = crate::slurm_cluster_spec::spec_to_json(&spec)
            .map_err(|e| Status::internal(format!("encode slurm spec: {e}")))?;
        let phase = if existing.is_some() {
            "applying"
        } else {
            "pending"
        };
        let status_json = serde_json::json!({
            "observedGeneration": 0,
            "phase": phase,
            "message": if existing.is_some() { "cluster update queued" } else { "cluster creation started" },
            "nodes": [],
        })
        .to_string();
        let row = SlurmClusterRow {
            name: name.clone(),
            spec_json,
            generation,
            munge_key,
            dbd_password,
            status_json,
            created_at: existing
                .as_ref()
                .map(|row| row.created_at.clone())
                .unwrap_or_default(),
            updated_at: String::new(),
        };
        self.db
            .upsert_slurm_cluster(&row)
            .map_err(|e| Status::internal(format!("storing slurm cluster: {e}")))?;

        let members = self.slurm_members(&spec)?;
        let node_rows: Vec<SlurmClusterNodeRow> = members
            .values()
            .map(|member| SlurmClusterNodeRow {
                node_id: member.node_id.clone(),
                cluster_name: name.clone(),
                roles: member.roles.join(","),
                applied_generation: 0,
                slurm_state: "unknown".into(),
                config_hash: String::new(),
                updated_at: String::new(),
            })
            .collect();
        self.db
            .replace_slurm_cluster_nodes(&name, &node_rows)
            .map_err(|e| Status::internal(format!("storing slurm membership: {e}")))?;
        let stored = self
            .db
            .get_slurm_cluster(&name)
            .map_err(|e| Status::internal(format!("reading slurm cluster: {e}")))?
            .ok_or_else(|| Status::internal("slurm cluster disappeared during create"))?;
        let nodes = self
            .db
            .list_slurm_cluster_nodes(&name)
            .map_err(|e| Status::internal(format!("listing slurm nodes: {e}")))?;
        self.log_replication_event(
            &actor,
            Some("CreateSlurmCluster"),
            if existing.is_some() {
                EVT_SLURM_CLUSTER_UPDATE
            } else {
                EVT_SLURM_CLUSTER_CREATE
            },
            &format!("slurm_cluster/{name}"),
            Self::slurm_replication_body(&stored, &nodes),
        );
        Ok(Response::new(controller_proto::CreateSlurmClusterResponse {
            success: true,
            message: format!(
                "SlurmCluster '{name}' {} at generation {generation}; the reconciler applies members in order dbd, controller, compute, login",
                if existing.is_some() { "updated" } else { "created" }
            ),
            cluster: Some(slurm_cluster_to_proto(&stored, &nodes)?),
            action: if existing.is_some() {
                controller_proto::ApplyAction::Updated as i32
            } else {
                controller_proto::ApplyAction::Created as i32
            },
            changed_fields,
        }))
    }

    async fn get_slurm_cluster(
        &self,
        request: Request<controller_proto::GetSlurmClusterRequest>,
    ) -> Result<Response<controller_proto::GetSlurmClusterResponse>, Status> {
        self.require_operator(&request, OperatorRole::ReadOnly)?;
        let name = validate_network_name(&request.into_inner().name)?;
        let row = self
            .db
            .get_slurm_cluster(&name)
            .map_err(|e| Status::internal(format!("reading slurm cluster: {e}")))?
            .ok_or_else(|| Status::not_found(format!("SlurmCluster '{name}' not found")))?;
        let nodes = self
            .db
            .list_slurm_cluster_nodes(&name)
            .map_err(|e| Status::internal(format!("listing slurm nodes: {e}")))?;
        Ok(Response::new(controller_proto::GetSlurmClusterResponse {
            cluster: Some(slurm_cluster_to_proto(&row, &nodes)?),
        }))
    }

    async fn list_slurm_clusters(
        &self,
        request: Request<controller_proto::ListSlurmClustersRequest>,
    ) -> Result<Response<controller_proto::ListSlurmClustersResponse>, Status> {
        self.require_operator(&request, OperatorRole::ReadOnly)?;
        let rows = self
            .db
            .list_slurm_clusters()
            .map_err(|e| Status::internal(format!("listing slurm clusters: {e}")))?;
        let mut clusters = Vec::new();
        for row in rows {
            let nodes = self
                .db
                .list_slurm_cluster_nodes(&row.name)
                .map_err(|e| Status::internal(format!("listing slurm nodes: {e}")))?;
            clusters.push(slurm_cluster_to_proto(&row, &nodes)?);
        }
        Ok(Response::new(controller_proto::ListSlurmClustersResponse {
            clusters,
        }))
    }

    async fn delete_slurm_cluster(
        &self,
        request: Request<controller_proto::DeleteSlurmClusterRequest>,
    ) -> Result<Response<controller_proto::DeleteSlurmClusterResponse>, Status> {
        self.require_operator(&request, OperatorRole::ClusterAdmin)?;
        let actor = Self::audit_actor(&request);
        let req = request.into_inner();
        let name = validate_network_name(&req.name)?;
        let row = self
            .db
            .get_slurm_cluster(&name)
            .map_err(|e| Status::internal(format!("reading slurm cluster: {e}")))?
            .ok_or_else(|| Status::not_found(format!("SlurmCluster '{name}' not found")))?;
        let nodes = self
            .db
            .list_slurm_cluster_nodes(&name)
            .map_err(|e| Status::internal(format!("listing slurm nodes: {e}")))?;
        if req.force {
            for node in &nodes {
                if node.roles.split(',').any(|r| r == "compute") {
                    if let Some(row) = self
                        .db
                        .get_node(&node.node_id)
                        .map_err(|e| Status::internal(format!("reading node: {e}")))?
                    {
                        if matches!(row.status.as_str(), "cordoned" | "draining" | "drained") {
                            let _ = self.db.update_node_status(&node.node_id, "ready");
                        }
                    }
                }
            }
            self.db
                .delete_slurm_cluster(&name)
                .map_err(|e| Status::internal(format!("deleting slurm cluster: {e}")))?;
            self.log_replication_event(
                &actor,
                Some("DeleteSlurmCluster"),
                EVT_SLURM_CLUSTER_DELETE,
                &format!("slurm_cluster/{name}"),
                Self::slurm_replication_body(&row, &nodes),
            );
            return Ok(Response::new(controller_proto::DeleteSlurmClusterResponse {
                success: true,
                message: format!(
                    "SlurmCluster '{name}' record dropped with force; unreachable members keep their units until they are rebuilt"
                ),
            }));
        }
        let mut status: serde_json::Value =
            serde_json::from_str(&row.status_json).unwrap_or_else(|_| serde_json::json!({}));
        status["phase"] = serde_json::Value::String("deleting".into());
        status["observedGeneration"] = serde_json::Value::Number(0.into());
        status["message"] = serde_json::Value::String("deletion queued".into());
        let mut deleting = row.clone();
        deleting.status_json = status.to_string();
        self.db
            .upsert_slurm_cluster(&deleting)
            .map_err(|e| Status::internal(format!("queueing slurm delete: {e}")))?;
        self.log_replication_event(
            &actor,
            Some("DeleteSlurmCluster"),
            EVT_SLURM_CLUSTER_DELETE,
            &format!("slurm_cluster/{name}"),
            Self::slurm_replication_body(&deleting, &nodes),
        );
        Ok(Response::new(controller_proto::DeleteSlurmClusterResponse {
            success: true,
            message: format!(
                "SlurmCluster '{name}' deletion queued; the reconciler disables Slurm on every member, waits for the units to stop, uncordons compute members, then drops the rows"
            ),
        }))
    }

    async fn rotate_slurm_munge_key(
        &self,
        request: Request<controller_proto::RotateSlurmMungeKeyRequest>,
    ) -> Result<Response<controller_proto::RotateSlurmMungeKeyResponse>, Status> {
        self.require_operator(&request, OperatorRole::ClusterAdmin)?;
        let actor = Self::audit_actor(&request);
        let name = validate_network_name(&request.into_inner().name)?;
        let key = slurm_random_bytes(1024)?;
        let generation = self
            .db
            .rotate_slurm_munge_key(&name, &key)
            .map_err(|e| Status::internal(format!("rotating munge key: {e}")))?
            .ok_or_else(|| Status::not_found(format!("SlurmCluster '{name}' not found")))?;
        let row = self
            .db
            .get_slurm_cluster(&name)
            .map_err(|e| Status::internal(format!("reading slurm cluster: {e}")))?
            .ok_or_else(|| Status::internal("slurm cluster disappeared during rotation"))?;
        let nodes = self
            .db
            .list_slurm_cluster_nodes(&name)
            .map_err(|e| Status::internal(format!("listing slurm nodes: {e}")))?;
        self.log_replication_event(
            &actor,
            Some("RotateSlurmMungeKey"),
            EVT_SLURM_CLUSTER_UPDATE,
            &format!("slurm_cluster/{name}"),
            Self::slurm_replication_body(&row, &nodes),
        );
        Ok(Response::new(controller_proto::RotateSlurmMungeKeyResponse {
            success: true,
            message: format!(
                "munge key rotation queued for SlurmCluster '{name}'; members restart munge in order dbd, controller, compute, login"
            ),
            generation,
        }))
    }

    async fn drain_slurm_node(
        &self,
        request: Request<controller_proto::DrainSlurmNodeRequest>,
    ) -> Result<Response<controller_proto::DrainSlurmNodeResponse>, Status> {
        self.require_operator(&request, OperatorRole::ClusterAdmin)?;
        let actor = Self::audit_actor(&request);
        let req = request.into_inner();
        let name = validate_network_name(&req.name)?;
        let reason = if req.reason.trim().is_empty() {
            "drained by kcore".to_string()
        } else {
            req.reason.replace(['\n', '\r'], " ")
        };
        let message = self
            .slurm_control_rpc(&name, &req.node_id, "drain", &reason)
            .await?;
        self.record_audit(
            &actor,
            "DrainSlurmNode",
            &format!("slurm_cluster/{name}/{}", req.node_id),
            &reason,
        );
        Ok(Response::new(controller_proto::DrainSlurmNodeResponse {
            success: true,
            message,
        }))
    }

    async fn resume_slurm_node(
        &self,
        request: Request<controller_proto::ResumeSlurmNodeRequest>,
    ) -> Result<Response<controller_proto::ResumeSlurmNodeResponse>, Status> {
        self.require_operator(&request, OperatorRole::ClusterAdmin)?;
        let actor = Self::audit_actor(&request);
        let req = request.into_inner();
        let name = validate_network_name(&req.name)?;
        let message = self
            .slurm_control_rpc(&name, &req.node_id, "resume", "")
            .await?;
        self.record_audit(
            &actor,
            "ResumeSlurmNode",
            &format!("slurm_cluster/{name}/{}", req.node_id),
            "",
        );
        Ok(Response::new(controller_proto::ResumeSlurmNodeResponse {
            success: true,
            message,
        }))
    }

    async fn plan_kubernetes_cluster(
        &self,
        request: Request<controller_proto::PlanKubernetesClusterRequest>,
    ) -> Result<Response<controller_proto::PlanKubernetesClusterResponse>, Status> {
        self.require_operator(&request, OperatorRole::ClusterAdmin)?;
        let req = request.into_inner();

        if req.name.trim().is_empty() {
            return Err(Status::invalid_argument("cluster name is required"));
        }

        let spec = req
            .spec
            .ok_or_else(|| Status::invalid_argument("spec is required"))?;

        // Validate spec
        let mut blocking_issues = Vec::new();
        let mut warnings = Vec::new();

        if spec.cluster_cidr.is_empty() {
            blocking_issues.push("cluster_cidr is required".to_string());
        }
        if spec.service_cidr.is_empty() {
            blocking_issues.push("service_cidr is required".to_string());
        }
        if spec.api_server_endpoint.is_empty() {
            blocking_issues.push("api_server_endpoint is required".to_string());
        }

        if spec.control_plane_nodes.is_empty() {
            blocking_issues.push("at least one control plane node is required".to_string());
        }

        // Check if cluster already exists
        let existing = self
            .db
            .get_kubernetes_cluster(&req.name)
            .map_err(internal_db)?;
        let action = if existing.is_some() {
            "update"
        } else {
            "create"
        };

        // Validate control plane nodes exist and are ready
        let mut node_plans = Vec::new();
        for cp_node in &spec.control_plane_nodes {
            let node = self.db.get_node(&cp_node.node_id).map_err(internal_db)?;
            match node {
                Some(n) if n.status == "ready" => {
                    node_plans.push(controller_proto::NodePlanResult {
                        node_id: cp_node.node_id.clone(),
                        preflight_passed: true,
                        issues: vec![],
                        nix_action: "build".to_string(),
                    });
                }
                Some(n) => {
                    node_plans.push(controller_proto::NodePlanResult {
                        node_id: cp_node.node_id.clone(),
                        preflight_passed: false,
                        issues: vec![format!("node status is {} (need ready)", n.status)],
                        nix_action: "no-change".to_string(),
                    });
                    blocking_issues.push(format!(
                        "control plane node {} is not ready",
                        cp_node.node_id
                    ));
                }
                None => {
                    blocking_issues
                        .push(format!("control plane node {} not found", cp_node.node_id));
                }
            }
        }

        // Validate worker nodes
        for worker in &spec.worker_nodes {
            let node = self.db.get_node(&worker.node_id).map_err(internal_db)?;
            match node {
                Some(n) if n.status == "ready" => {
                    node_plans.push(controller_proto::NodePlanResult {
                        node_id: worker.node_id.clone(),
                        preflight_passed: true,
                        issues: vec![],
                        nix_action: "build".to_string(),
                    });
                }
                Some(n) => {
                    warnings.push(format!(
                        "worker node {} status is {}",
                        worker.node_id, n.status
                    ));
                }
                None => {
                    warnings.push(format!("worker node {} not found", worker.node_id));
                }
            }
        }

        let local = match Self::local_provisioner_of(&req.name, &spec) {
            Ok(provisioner) => Some(provisioner),
            Err(err) => {
                blocking_issues.push(err.message().to_string());
                None
            }
        };
        let rendered_nix_preview = match local.as_ref().filter(|provisioner| provisioner.enabled) {
            Some(provisioner) => format!(
                "# Kubernetes configuration will be generated\n{}",
                crate::kubernetes_local::manifests_at(provisioner, &spec.api_server_endpoint)
            ),
            None => "# Kubernetes configuration will be generated".to_string(),
        };

        Ok(Response::new(
            controller_proto::PlanKubernetesClusterResponse {
                action: action.to_string(),
                blocking_issues,
                warnings,
                node_plans,
                rendered_nix_preview,
            },
        ))
    }

    async fn create_kubernetes_cluster(
        &self,
        request: Request<controller_proto::CreateKubernetesClusterRequest>,
    ) -> Result<Response<controller_proto::CreateKubernetesClusterResponse>, Status> {
        self.require_operator(&request, OperatorRole::ClusterAdmin)?;
        let actor = Self::audit_actor(&request);
        let req = request.into_inner();

        if !req.confirm {
            return Err(Status::failed_precondition("confirm must be true"));
        }

        if req.name.trim().is_empty() {
            return Err(Status::invalid_argument("cluster name is required"));
        }

        let spec = req
            .spec
            .ok_or_else(|| Status::invalid_argument("spec is required"))?;

        // Check if cluster already exists
        if let Some(_existing) = self
            .db
            .get_kubernetes_cluster(&req.name)
            .map_err(internal_db)?
        {
            return Err(Status::already_exists(format!(
                "cluster {} already exists",
                req.name
            )));
        }

        let provisioner = Self::local_provisioner_of(&req.name, &spec)?;

        // Store cluster in database
        let mut cluster_row = crate::db::KubernetesClusterRow {
            name: req.name.clone(),
            generation: 1,
            cluster_cidr: spec.cluster_cidr.clone(),
            service_cidr: spec.service_cidr.clone(),
            cluster_dns: spec.cluster_dns.clone(),
            api_server_endpoint: spec.api_server_endpoint.clone(),
            cni: stored_cni(&spec.api_server_endpoint, &spec.cni),
            phase: "bootstrapping".to_string(),
            status_message: "cluster creation started".to_string(),
            ready_control_planes: 0,
            ready_workers: 0,
            created_at: String::new(),
            updated_at: String::new(),
        };

        self.db
            .upsert_kubernetes_cluster(&cluster_row)
            .map_err(internal_db)?;

        let config_row = Self::kubernetes_config_row(&req.name, &spec, &provisioner);
        self.db
            .upsert_kubernetes_cluster_config(&config_row)
            .map_err(internal_db)?;

        // Store nodes
        for cp_node in &spec.control_plane_nodes {
            let node_row = crate::db::KubernetesClusterNodeRow {
                cluster_name: req.name.clone(),
                node_id: cp_node.node_id.clone(),
                role: "control-plane".to_string(),
                advertise_address: cp_node.advertise_address.clone(),
                iface: cp_node.iface.clone(),
                initial_cluster: true,
                join_existing: false,
                kubelet_ready: false,
                pending_join: false,
                pending_removal: false,
                k8s_node_name: String::new(),
            };
            self.db
                .upsert_kubernetes_cluster_node(&node_row)
                .map_err(internal_db)?;
        }

        for worker in &spec.worker_nodes {
            let node_row = crate::db::KubernetesClusterNodeRow {
                cluster_name: req.name.clone(),
                node_id: worker.node_id.clone(),
                role: "worker".to_string(),
                advertise_address: worker.advertise_address.clone(),
                iface: worker.iface.clone(),
                initial_cluster: false,
                join_existing: true,
                kubelet_ready: false,
                pending_join: false,
                pending_removal: false,
                k8s_node_name: String::new(),
            };
            self.db
                .upsert_kubernetes_cluster_node(&node_row)
                .map_err(internal_db)?;
        }

        if let Err(err) = self
            .reconcile_local_volumes(&actor, &req.name, &provisioner)
            .await
        {
            warn!(
                cluster = %req.name,
                error = %err,
                "local volumes were not reconciled"
            );
            cluster_row.phase = "degraded".to_string();
            cluster_row.status_message = err.message().to_string();
            self.db
                .upsert_kubernetes_cluster(&cluster_row)
                .map_err(internal_db)?;
        }

        info!(
            cluster = %req.name,
            "kubernetes cluster created; the orchestrator renders node nix with PKI"
        );

        let cluster = self.build_kubernetes_cluster_proto(&req.name)?;
        Ok(Response::new(
            controller_proto::CreateKubernetesClusterResponse {
                cluster: Some(cluster),
            },
        ))
    }

    async fn update_kubernetes_cluster(
        &self,
        request: Request<controller_proto::UpdateKubernetesClusterRequest>,
    ) -> Result<Response<controller_proto::UpdateKubernetesClusterResponse>, Status> {
        self.require_operator(&request, OperatorRole::ClusterAdmin)?;
        let actor = Self::audit_actor(&request);
        let req = request.into_inner();

        if !req.confirm {
            return Err(Status::failed_precondition("confirm must be true"));
        }

        let existing = self
            .db
            .get_kubernetes_cluster(&req.name)
            .map_err(internal_db)?
            .ok_or_else(|| Status::not_found(format!("cluster {} not found", req.name)))?;

        let spec = req
            .spec
            .ok_or_else(|| Status::invalid_argument("spec is required"))?;
        let provisioner = Self::local_provisioner_of(&req.name, &spec)?;
        let existing_members = self
            .db
            .list_kubernetes_cluster_nodes(&req.name)
            .map_err(internal_db)?;
        let mut desired = Vec::new();
        for node in &spec.control_plane_nodes {
            desired.push((node.node_id.clone(), "control-plane".to_string()));
        }
        for node in &spec.worker_nodes {
            desired.push((node.node_id.clone(), "worker".to_string()));
        }
        let existing_pairs: Vec<(String, String)> = existing_members
            .iter()
            .map(|row| (row.node_id.clone(), row.role.clone()))
            .collect();
        let crate::kubernetes_lifecycle::MembershipDelta { add, remove } =
            crate::kubernetes_lifecycle::membership_delta(&existing_pairs, &desired)
                .map_err(Status::invalid_argument)?;
        let add_specs: Vec<controller_proto::KubernetesNodeSpec> = spec
            .control_plane_nodes
            .iter()
            .chain(spec.worker_nodes.iter())
            .filter(|node| add.iter().any(|(id, _)| id == &node.node_id))
            .cloned()
            .collect();
        let membership_changed =
            self.stage_kubernetes_membership(&req.name, &add_specs, &remove)?;

        // Update cluster
        let mut cluster_row = crate::db::KubernetesClusterRow {
            name: req.name.clone(),
            generation: existing.generation + 1,
            cluster_cidr: spec.cluster_cidr.clone(),
            service_cidr: spec.service_cidr.clone(),
            cluster_dns: spec.cluster_dns.clone(),
            api_server_endpoint: spec.api_server_endpoint.clone(),
            cni: stored_cni(&spec.api_server_endpoint, &spec.cni),
            phase: "updating".to_string(),
            status_message: "cluster update in progress".to_string(),
            ready_control_planes: existing.ready_control_planes,
            ready_workers: existing.ready_workers,
            created_at: existing.created_at,
            updated_at: String::new(),
        };

        self.db
            .upsert_kubernetes_cluster(&cluster_row)
            .map_err(internal_db)?;
        let config_row = Self::kubernetes_config_row(&req.name, &spec, &provisioner);
        self.db
            .upsert_kubernetes_cluster_config(&config_row)
            .map_err(internal_db)?;
        let volume_error = match self
            .reconcile_local_volumes(&actor, &req.name, &provisioner)
            .await
        {
            Ok(()) => None,
            Err(err) => {
                warn!(
                    cluster = %req.name,
                    error = %err,
                    "local volumes were not reconciled"
                );
                Some(err.message().to_string())
            }
        };
        // A config change has to reach the nodes. Leaving it in the database
        // is how a cgroup, VIP, or provisioner fix stayed off the machines.
        let orchestrator = crate::kubernetes_orchestrator::KubernetesOrchestrator::new(
            self.db.clone(),
            self.clients.clone(),
        );
        let deploy_error = match orchestrator
            .redeploy_node_certificates(
                &req.name,
                &cluster_row.api_server_endpoint,
                &cluster_row.service_cidr,
            )
            .await
        {
            Ok(()) => None,
            Err(err) => {
                warn!(
                    cluster = %req.name,
                    error = %err,
                    "kubernetes node configuration was not deployed"
                );
                Some(format!("{err:#}"))
            }
        };
        if membership_changed {
            cluster_row.phase = "scaling".to_string();
            cluster_row.status_message = match deploy_error.or(volume_error) {
                Some(message) => format!("cluster scaling in progress; {message}"),
                None => "cluster scaling in progress".to_string(),
            };
        } else if let Some(message) = deploy_error.or(volume_error) {
            cluster_row.phase = "degraded".to_string();
            cluster_row.status_message = message;
        } else {
            // A ready cluster stays ready. A degraded cluster is the previous
            // attempt's failure; this attempt deployed, so it is ready again.
            let recovered = matches!(existing.phase.as_str(), "updating" | "degraded");
            cluster_row.phase = if recovered {
                "ready".to_string()
            } else {
                existing.phase.clone()
            };
            cluster_row.status_message = if spec.schedule_on_control_plane {
                "control-plane nodes accept workloads; node configuration deployed".to_string()
            } else if recovered {
                "cluster configuration deployed".to_string()
            } else {
                format!("{}; node configuration deployed", existing.status_message)
            };
        }
        self.db
            .upsert_kubernetes_cluster(&cluster_row)
            .map_err(internal_db)?;

        let cluster = self.build_kubernetes_cluster_proto(&req.name)?;
        Ok(Response::new(
            controller_proto::UpdateKubernetesClusterResponse {
                cluster: Some(cluster),
            },
        ))
    }

    async fn get_kubernetes_cluster(
        &self,
        request: Request<controller_proto::GetKubernetesClusterRequest>,
    ) -> Result<Response<controller_proto::GetKubernetesClusterResponse>, Status> {
        self.require_operator(&request, OperatorRole::ReadOnly)?;
        let req = request.into_inner();

        let cluster = self.build_kubernetes_cluster_proto(&req.name)?;
        Ok(Response::new(
            controller_proto::GetKubernetesClusterResponse {
                cluster: Some(cluster),
            },
        ))
    }

    async fn list_kubernetes_clusters(
        &self,
        request: Request<controller_proto::ListKubernetesClustersRequest>,
    ) -> Result<Response<controller_proto::ListKubernetesClustersResponse>, Status> {
        self.require_operator(&request, OperatorRole::ReadOnly)?;

        let rows = self.db.list_kubernetes_clusters().map_err(internal_db)?;
        let mut clusters = Vec::new();

        for row in rows {
            if let Ok(cluster) = self.build_kubernetes_cluster_proto(&row.name) {
                clusters.push(cluster);
            }
        }

        Ok(Response::new(
            controller_proto::ListKubernetesClustersResponse { clusters },
        ))
    }

    async fn delete_kubernetes_cluster(
        &self,
        request: Request<controller_proto::DeleteKubernetesClusterRequest>,
    ) -> Result<Response<controller_proto::DeleteKubernetesClusterResponse>, Status> {
        self.require_operator(&request, OperatorRole::ClusterAdmin)?;
        let req = request.into_inner();

        if !req.confirm {
            return Err(Status::failed_precondition("confirm must be true"));
        }

        let deleted = self
            .db
            .delete_kubernetes_cluster(&req.name)
            .map_err(internal_db)?;

        if !deleted {
            return Err(Status::not_found(format!("cluster {} not found", req.name)));
        }

        info!(cluster = %req.name, "kubernetes cluster deleted");

        Ok(Response::new(
            controller_proto::DeleteKubernetesClusterResponse {
                message: format!("cluster {} deleted", req.name),
            },
        ))
    }

    async fn scale_kubernetes_cluster(
        &self,
        request: Request<controller_proto::ScaleKubernetesClusterRequest>,
    ) -> Result<Response<controller_proto::ScaleKubernetesClusterResponse>, Status> {
        self.require_operator(&request, OperatorRole::ClusterAdmin)?;
        let req = request.into_inner();

        if !req.confirm {
            return Err(Status::failed_precondition("confirm must be true"));
        }

        let existing = self
            .db
            .get_kubernetes_cluster(&req.name)
            .map_err(internal_db)?
            .ok_or_else(|| Status::not_found(format!("cluster {} not found", req.name)))?;

        let changed =
            self.stage_kubernetes_membership(&req.name, &req.add_nodes, &req.remove_node_ids)?;
        if changed {
            let cluster_row = crate::db::KubernetesClusterRow {
                generation: existing.generation + 1,
                phase: "scaling".to_string(),
                status_message: "cluster scaling in progress".to_string(),
                ..existing
            };
            self.db
                .upsert_kubernetes_cluster(&cluster_row)
                .map_err(internal_db)?;
        }

        let cluster = self.build_kubernetes_cluster_proto(&req.name)?;
        Ok(Response::new(
            controller_proto::ScaleKubernetesClusterResponse {
                cluster: Some(cluster),
            },
        ))
    }

    async fn get_kubernetes_cluster_health(
        &self,
        request: Request<controller_proto::GetKubernetesClusterHealthRequest>,
    ) -> Result<Response<controller_proto::GetKubernetesClusterHealthResponse>, Status> {
        self.require_operator(&request, OperatorRole::ReadOnly)?;
        let req = request.into_inner();

        let cluster = self
            .db
            .get_kubernetes_cluster(&req.name)
            .map_err(internal_db)?
            .ok_or_else(|| Status::not_found(format!("cluster {} not found", req.name)))?;
        let schedule_on_control_plane = self
            .db
            .get_kubernetes_cluster_config(&req.name)
            .map_err(internal_db)?
            .map(|config| config.schedule_on_control_plane)
            .unwrap_or(false);
        let mut control_plane_ready_budget =
            if schedule_on_control_plane && cluster.phase == "ready" {
                cluster.ready_control_planes
            } else {
                0
            };

        let nodes = self
            .db
            .list_kubernetes_cluster_nodes(&req.name)
            .map_err(internal_db)?;

        let mut node_health = Vec::new();
        let mut ready_control_planes = 0;
        let mut total_control_planes = 0;
        let mut ready_workers = 0;
        let mut total_workers = 0;

        for node in &nodes {
            let control_plane = node.role == "control-plane";
            if control_plane {
                total_control_planes += 1;
                if node.kubelet_ready {
                    ready_control_planes += 1;
                }
            }
            if !control_plane || schedule_on_control_plane {
                total_workers += 1;
                let ready = if node.kubelet_ready {
                    true
                } else if control_plane && control_plane_ready_budget > 0 {
                    control_plane_ready_budget -= 1;
                    true
                } else {
                    false
                };
                if ready {
                    ready_workers += 1;
                }
            }

            node_health.push(controller_proto::NodeHealth {
                node_id: node.node_id.clone(),
                kubernetes_status: if node.kubelet_ready {
                    "Ready"
                } else {
                    "NotReady"
                }
                .to_string(),
                kubelet_active: node.kubelet_ready,
                etcd_active: node.role == "control-plane",
                api_server_active: node.role == "control-plane",
            });
        }

        let overall_health =
            if ready_control_planes == total_control_planes && ready_workers == total_workers {
                "healthy"
            } else if ready_control_planes > 0 {
                "degraded"
            } else {
                "critical"
            };

        Ok(Response::new(
            controller_proto::GetKubernetesClusterHealthResponse {
                overall_health: overall_health.to_string(),
                ready_control_planes,
                total_control_planes,
                ready_workers,
                total_workers,
                etcd_healthy: ready_control_planes > 0,
                etcd_member_count: total_control_planes,
                api_server_reachable: ready_control_planes > 0,
                node_health,
            },
        ))
    }

    async fn classify_disk_layout(
        &self,
        request: Request<controller_proto::ClassifyDiskLayoutRequest>,
    ) -> Result<Response<controller_proto::ClassifyDiskLayoutResponse>, Status> {
        self.require_operator(&request, OperatorRole::ReadOnly)?;
        self.classify_disk_layout_impl(request.into_inner()).await
    }
}

// Helper methods for Kubernetes cluster management
impl ControllerService {
    fn stage_kubernetes_membership(
        &self,
        cluster_name: &str,
        add_nodes: &[controller_proto::KubernetesNodeSpec],
        remove_node_ids: &[String],
    ) -> Result<bool, Status> {
        let current = self
            .db
            .list_kubernetes_cluster_nodes(cluster_name)
            .map_err(internal_db)?;
        for node in add_nodes {
            if node.role != "control-plane" && node.role != "worker" {
                return Err(Status::invalid_argument(format!(
                    "node {} role must be control-plane or worker",
                    node.node_id
                )));
            }
            match self.db.get_node(&node.node_id).map_err(internal_db)? {
                None => {
                    let message = format!("node {} is not an approved kcore node", node.node_id);
                    self.db
                        .set_kubernetes_cluster_status(cluster_name, "scaling", &message)
                        .map_err(internal_db)?;
                    return Err(Status::failed_precondition(message));
                }
                Some(row) if row.approval_status != "approved" => {
                    let message = format!("node {} is not an approved kcore node", node.node_id);
                    self.db
                        .set_kubernetes_cluster_status(cluster_name, "scaling", &message)
                        .map_err(internal_db)?;
                    return Err(Status::failed_precondition(message));
                }
                Some(_) => {}
            }
        }
        let snapshots: Vec<crate::kubernetes_lifecycle::MemberSnapshot> = current
            .iter()
            .map(|row| crate::kubernetes_lifecycle::MemberSnapshot {
                node_id: row.node_id.clone(),
                role: row.role.clone(),
                pending_join: row.pending_join,
                pending_removal: row.pending_removal,
                kubelet_ready: row.kubelet_ready,
                kcore_exists: true,
                kcore_ready: true,
                in_etcd: row.role != "worker" && !row.pending_join,
                etcd_healthy: row.role != "worker" && !row.pending_join,
                in_api: row.kubelet_ready,
                api_ready: row.kubelet_ready,
                registered_name: row.k8s_node_name.clone(),
            })
            .collect();
        for node_id in remove_node_ids {
            if let Some(message) =
                crate::kubernetes_lifecycle::etcd_removal_refusal(&snapshots, node_id)
            {
                return Err(Status::failed_precondition(message));
            }
        }

        let mut changed = false;
        for node in add_nodes {
            if let Some(existing) = current.iter().find(|row| row.node_id == node.node_id) {
                if crate::kubernetes_lifecycle::scale_add_noop(
                    &crate::kubernetes_lifecycle::MemberSnapshot {
                        node_id: existing.node_id.clone(),
                        role: existing.role.clone(),
                        pending_join: existing.pending_join,
                        pending_removal: existing.pending_removal,
                        kubelet_ready: existing.kubelet_ready,
                        kcore_exists: true,
                        kcore_ready: true,
                        in_etcd: existing.role != "worker",
                        etcd_healthy: existing.role != "worker",
                        in_api: existing.kubelet_ready,
                        api_ready: existing.kubelet_ready,
                        registered_name: existing.k8s_node_name.clone(),
                    },
                ) {
                    continue;
                }
            }
            let kcore = self
                .db
                .get_node(&node.node_id)
                .map_err(internal_db)?
                .ok_or_else(|| Status::failed_precondition("node disappeared"))?;
            if kcore.status != "ready" {
                self.db
                    .set_kubernetes_cluster_status(
                        cluster_name,
                        "scaling",
                        &format!("node {} is not ready", node.node_id),
                    )
                    .map_err(internal_db)?;
            }
            self.db
                .upsert_kubernetes_cluster_node(&crate::db::KubernetesClusterNodeRow {
                    cluster_name: cluster_name.to_string(),
                    node_id: node.node_id.clone(),
                    role: node.role.clone(),
                    advertise_address: node.advertise_address.clone(),
                    iface: node.iface.clone(),
                    initial_cluster: false,
                    join_existing: true,
                    kubelet_ready: false,
                    pending_join: true,
                    pending_removal: false,
                    k8s_node_name: current
                        .iter()
                        .find(|row| row.node_id == node.node_id)
                        .map(|row| row.k8s_node_name.clone())
                        .unwrap_or_default(),
                })
                .map_err(internal_db)?;
            changed = true;
        }
        for node_id in remove_node_ids {
            let Some(existing) = current.iter().find(|row| row.node_id == *node_id) else {
                continue;
            };
            if existing.pending_removal {
                changed = true;
                continue;
            }
            let mut row = existing.clone();
            row.pending_removal = true;
            self.db
                .upsert_kubernetes_cluster_node(&row)
                .map_err(internal_db)?;
            changed = true;
        }
        Ok(changed)
    }

    fn kubernetes_members(
        spec: &controller_proto::KubernetesClusterSpec,
    ) -> std::collections::BTreeSet<String> {
        spec.control_plane_nodes
            .iter()
            .chain(spec.worker_nodes.iter())
            .map(|node| node.node_id.trim().to_string())
            .filter(|id| !id.is_empty())
            .collect()
    }

    fn local_provisioner_of(
        cluster: &str,
        spec: &controller_proto::KubernetesClusterSpec,
    ) -> Result<crate::kubernetes_local::LocalProvisioner, Status> {
        let Some(declared) = spec.local_provisioner.as_ref() else {
            return Ok(crate::kubernetes_local::LocalProvisioner::disabled());
        };
        let volumes: Vec<crate::kubernetes_local::VolumeDecl<'_>> = declared
            .volumes
            .iter()
            .map(|volume| crate::kubernetes_local::VolumeDecl {
                name: volume.name.as_str(),
                node_id: volume.node_id.as_str(),
                storage_class: volume.storage_class.as_str(),
                size_bytes: volume.size_bytes,
                fs_type: volume.fs_type.as_str(),
                mount_path: volume.mount_path.as_str(),
                role: volume.role.as_str(),
                volume_group: volume.volume_group.as_str(),
            })
            .collect();
        crate::kubernetes_local::normalize(
            cluster,
            &Self::kubernetes_members(spec),
            declared.enabled,
            &declared.storage_class_name,
            &declared.mode,
            &volumes,
        )
        .map_err(Status::invalid_argument)
    }

    fn kubernetes_config_row(
        name: &str,
        spec: &controller_proto::KubernetesClusterSpec,
        provisioner: &crate::kubernetes_local::LocalProvisioner,
    ) -> crate::db::KubernetesClusterConfigRow {
        let ha_config = spec.high_availability.as_ref();
        let cilium_config = spec.cilium_config.as_ref();
        crate::db::KubernetesClusterConfigRow {
            cluster_name: name.to_string(),
            ha_enabled: ha_config.map(|c| c.enabled).unwrap_or(false),
            vip_address: ha_config.map(|c| c.vip_address.clone()).unwrap_or_default(),
            vip_interface: ha_config
                .map(|c| c.vip_interface.clone())
                .unwrap_or_default(),
            cilium_mode: cilium_config
                .map(|c| c.mode.clone())
                .unwrap_or_else(|| "native-routing".to_string()),
            cilium_mtu: cilium_config.map(|c| c.mtu).unwrap_or(1500),
            cilium_tunnel: cilium_config
                .map(|c| c.tunnel.clone())
                .unwrap_or_else(|| "disabled".to_string()),
            cilium_auto_direct_routes: cilium_config
                .map(|c| c.auto_direct_node_routes)
                .unwrap_or(true),
            cilium_lb_ip_pools: cilium_config
                .map(|c| c.load_balancer_ip_pools.join(","))
                .unwrap_or_default(),
            local_provisioner_json: crate::kubernetes_local::encode(provisioner),
            schedule_on_control_plane: spec.schedule_on_control_plane,
        }
    }

    async fn reconcile_local_volumes(
        &self,
        actor: &str,
        cluster: &str,
        provisioner: &crate::kubernetes_local::LocalProvisioner,
    ) -> Result<(), Status> {
        if !provisioner.enabled {
            return Ok(());
        }
        for volume in &provisioner.volumes {
            self.reconcile_one_local_volume(actor, cluster, volume)
                .await?;
        }
        Ok(())
    }

    async fn reconcile_one_local_volume(
        &self,
        actor: &str,
        cluster: &str,
        volume: &crate::kubernetes_local::LocalVolume,
    ) -> Result<(), Status> {
        let vol_name =
            crate::kubernetes_local::controller_volume_name(cluster, &volume.node_id, &volume.name);
        let node = self
            .db
            .get_node(&volume.node_id)
            .map_err(|e| Status::internal(e.to_string()))?
            .ok_or_else(|| Status::not_found(format!("node '{}' not found", volume.node_id)))?;
        if node.storage_backend != volume.storage_class {
            return Err(Status::failed_precondition(format!(
                "node {} storage backend is {}, not {}",
                node.id, node.storage_backend, volume.storage_class
            )));
        }
        let existing = self
            .db
            .get_volume_by_name(&vol_name)
            .map_err(|e| Status::internal(e.to_string()))?;
        let mut row = if let Some(existing) = existing {
            if existing.host_node_id != node.id || existing.storage_class != volume.storage_class {
                return Err(Status::failed_precondition(format!(
                    "volume {vol_name} already belongs to {}/{}",
                    existing.host_node_id, existing.storage_class
                )));
            }
            if volume.size_bytes < existing.size_bytes {
                return Err(Status::invalid_argument(format!(
                    "volume {vol_name} is {} bytes; shrinking it is not part of a cluster update",
                    existing.size_bytes
                )));
            }
            existing
        } else {
            self.create_or_adopt_host_volume(actor, &vol_name, &volume.name, &node, volume)
                .await?
        };
        if volume.storage_class == "lvm" {
            self.ensure_lvm_role(&node, &volume.name, &volume.role)
                .await?;
        }
        if volume.role == "pool" && (!row.fs_type.is_empty() || !row.mount_point.is_empty()) {
            return Err(Status::failed_precondition(format!(
                "volume {vol_name} is a formatted filesystem; a thin pool needs its own name"
            )));
        }
        if volume.size_bytes > row.size_bytes {
            let device = crate::host_volume::device_path(&row.storage_class, &row.pool, &row.image)
                .map_err(Status::failed_precondition)?;
            let mut storage = self.connect_node_storage(&node).await?;
            let resized = storage
                .resize_volume(node_proto::ResizeVolumeRequest {
                    backend_handle: device,
                    size_bytes: volume.size_bytes,
                    allow_shrink: false,
                })
                .await
                .map_err(|e| Status::internal(format!("resizing {vol_name}: {e}")))?
                .into_inner();
            row.size_bytes = if resized.size_bytes > 0 {
                resized.size_bytes
            } else {
                volume.size_bytes
            };
            row.generation += 1;
            self.db
                .upsert_volume(&row)
                .map_err(|e| Status::internal(e.to_string()))?;
        }
        if volume.role == "pool" {
            return Ok(());
        }
        if row.fs_type.is_empty() {
            let device = crate::host_volume::device_path(&row.storage_class, &row.pool, &row.image)
                .map_err(Status::failed_precondition)?;
            let mut storage = self.connect_node_storage(&node).await?;
            match storage
                .format_volume(node_proto::FormatVolumeRequest {
                    backend_handle: device,
                    fs_type: volume.fs_type.clone(),
                    force: false,
                })
                .await
            {
                Ok(_) => {
                    row.fs_type = volume.fs_type.clone();
                    row.generation += 1;
                    self.db
                        .upsert_volume(&row)
                        .map_err(|e| Status::internal(e.to_string()))?;
                }
                Err(e) if e.message().contains("already has a") => {
                    row.fs_type = volume.fs_type.clone();
                    row.generation += 1;
                    self.db
                        .upsert_volume(&row)
                        .map_err(|e| Status::internal(e.to_string()))?;
                }
                Err(e) if e.code() == tonic::Code::Unimplemented => {
                    return Err(Status::failed_precondition(format!(
                        "node {} cannot format volumes; install a node agent that implements FormatVolume",
                        node.id
                    )));
                }
                Err(e) => {
                    return Err(Status::internal(format!("formatting {vol_name}: {e}")));
                }
            }
        } else if row.fs_type != volume.fs_type {
            return Err(Status::failed_precondition(format!(
                "volume {vol_name} is {}, not {}",
                row.fs_type, volume.fs_type
            )));
        }
        if row.mount_point.is_empty() {
            let device = crate::host_volume::device_path(&row.storage_class, &row.pool, &row.image)
                .map_err(Status::failed_precondition)?;
            let mut storage = self.connect_node_storage(&node).await?;
            let mounted = storage
                .mount_volume(node_proto::MountVolumeRequest {
                    backend_handle: device,
                    mount_point: volume.mount_path.clone(),
                    fs_type: volume.fs_type.clone(),
                    persistent: true,
                })
                .await
                .map_err(|e| {
                    if e.code() == tonic::Code::Unimplemented {
                        Status::failed_precondition(format!(
                            "node {} cannot mount volumes; install a node agent that implements MountVolume",
                            node.id
                        ))
                    } else {
                        Status::internal(format!("mounting {vol_name}: {e}"))
                    }
                })?
                .into_inner();
            row.mount_point = if mounted.mount_point.is_empty() {
                volume.mount_path.clone()
            } else {
                mounted.mount_point
            };
            row.generation += 1;
            self.db
                .upsert_volume(&row)
                .map_err(|e| Status::internal(e.to_string()))?;
            self.log_replication_event(
                actor,
                Some("volume.update"),
                EVT_VOLUME_UPDATE,
                &format!("volume/{vol_name}"),
                self.host_volume_event(&row),
            );
        } else if row.mount_point != volume.mount_path {
            return Err(Status::failed_precondition(format!(
                "volume {vol_name} is mounted at {}, not {}",
                row.mount_point, volume.mount_path
            )));
        }
        Ok(())
    }

    async fn create_or_adopt_host_volume(
        &self,
        actor: &str,
        vol_name: &str,
        lv_name: &str,
        node: &crate::db::NodeRow,
        volume: &crate::kubernetes_local::LocalVolume,
    ) -> Result<crate::db::VolumeRow, Status> {
        let mut storage = self.connect_node_storage(node).await?;
        let mut parameters = HashMap::new();
        if volume.role == "pool" {
            parameters.insert("lvm-type".to_string(), "thin-pool".to_string());
        }
        let created = storage
            .create_volume(node_proto::CreateVolumeRequest {
                volume_id: vol_name.to_string(),
                storage_class: volume.storage_class.clone(),
                size_bytes: volume.size_bytes,
                parameters,
                lv_name: lv_name.to_string(),
            })
            .await;
        let handle = match created {
            Ok(response) => response.into_inner().backend_handle,
            Err(e) if volume.storage_class == "lvm" && e.to_string().contains("already exists") => {
                self.adopt_lvm_handle(node, lv_name).await?
            }
            Err(e) => {
                return Err(Status::internal(format!(
                    "creating {} volume {lv_name} on {}: {e}",
                    volume.storage_class, node.id
                )));
            }
        };
        let (pool, image) =
            crate::host_volume::split_backend_handle(&volume.storage_class, &handle)
                .map_err(Status::internal)?;
        let mut row = crate::db::VolumeRow::new_data(vol_name, volume.size_bytes);
        row.storage_class = volume.storage_class.clone();
        row.host_node_id = node.id.clone();
        row.pool = pool;
        row.image = image;
        self.db
            .upsert_volume(&row)
            .map_err(|e| Status::internal(format!("storing volume: {e}")))?;
        self.log_replication_event(
            actor,
            Some("volume.create"),
            EVT_VOLUME_CREATE,
            &format!("volume/{vol_name}"),
            self.host_volume_event(&row),
        );
        Ok(row)
    }

    async fn ensure_lvm_role(
        &self,
        node: &crate::db::NodeRow,
        lv_name: &str,
        role: &str,
    ) -> Result<(), Status> {
        let lv = self.lvm_logical_volume(node, lv_name).await?;
        let thin_pool = lv.attr.starts_with('t');
        if role == "pool" && !thin_pool {
            return Err(Status::failed_precondition(format!(
                "logical volume {lv_name} on {} is not a thin pool (attr {}); choose a new name so the existing volume is left intact",
                node.id, lv.attr
            )));
        }
        if role != "pool" && thin_pool {
            return Err(Status::failed_precondition(format!(
                "logical volume {lv_name} on {} is a thin pool; declare mode lvm to carve formatted volumes from it",
                node.id
            )));
        }
        Ok(())
    }

    async fn lvm_logical_volume(
        &self,
        node: &crate::db::NodeRow,
        lv_name: &str,
    ) -> Result<node_proto::LvmLogicalVolumeInfo, Status> {
        if self.clients.get_admin(&node.address).is_none() {
            self.clients
                .connect(&node.address)
                .await
                .map_err(|e| Status::unavailable(format!("connecting to {}: {e}", node.id)))?;
        }
        let mut admin = self.clients.get_admin(&node.address).ok_or_else(|| {
            Status::unavailable(format!("admin client unavailable for {}", node.id))
        })?;
        let info = admin
            .get_lvm_info(node_proto::GetLvmInfoRequest {})
            .await
            .map_err(|e| Status::internal(format!("reading LVM on {}: {e}", node.id)))?
            .into_inner();
        info.logical_volumes
            .into_iter()
            .find(|lv| lv.name == lv_name)
            .ok_or_else(|| {
                Status::failed_precondition(format!(
                    "logical volume {lv_name} was not found in LVM inventory on {}",
                    node.id
                ))
            })
    }

    async fn adopt_lvm_handle(
        &self,
        node: &crate::db::NodeRow,
        lv_name: &str,
    ) -> Result<String, Status> {
        let path = self.lvm_logical_volume(node, lv_name).await?.path;
        if path.is_empty() {
            return Err(Status::already_exists(format!(
                "logical volume {lv_name} already exists on {} but has no device path",
                node.id
            )));
        }
        Ok(path)
    }

    fn build_kubernetes_cluster_proto(
        &self,
        name: &str,
    ) -> Result<controller_proto::KubernetesCluster, Status> {
        let row = self
            .db
            .get_kubernetes_cluster(name)
            .map_err(internal_db)?
            .ok_or_else(|| Status::not_found(format!("cluster {} not found", name)))?;

        let config = self
            .db
            .get_kubernetes_cluster_config(name)
            .map_err(internal_db)?;
        let nodes = self
            .db
            .list_kubernetes_cluster_nodes(name)
            .map_err(internal_db)?;

        let mut control_plane_nodes = Vec::new();
        let mut worker_nodes = Vec::new();

        for node in nodes {
            let node_spec = controller_proto::KubernetesNodeSpec {
                node_id: node.node_id.clone(),
                role: node.role.clone(),
                advertise_address: node.advertise_address.clone(),
                iface: node.iface.clone(),
                initial_cluster: node.initial_cluster,
                join_existing: node.join_existing,
            };

            if node.role == "control-plane" {
                control_plane_nodes.push(node_spec);
            } else {
                worker_nodes.push(node_spec);
            }
        }

        let cilium_config = config.as_ref().map(|c| controller_proto::CiliumConfig {
            mode: c.cilium_mode.clone(),
            mtu: c.cilium_mtu,
            tunnel: c.cilium_tunnel.clone(),
            auto_direct_node_routes: c.cilium_auto_direct_routes,
            load_balancer_ip_pools: c
                .cilium_lb_ip_pools
                .split(',')
                .filter(|s| !s.is_empty())
                .map(|s| s.to_string())
                .collect(),
        });

        let ha_config = config
            .as_ref()
            .map(|c| controller_proto::HighAvailabilityConfig {
                enabled: c.ha_enabled,
                vip_address: c.vip_address.clone(),
                vip_interface: c.vip_interface.clone(),
                method: "keepalived".to_string(),
            });
        let local_provisioner = config
            .as_ref()
            .map(|c| crate::kubernetes_local::decode(&c.local_provisioner_json))
            .transpose()
            .map_err(Status::internal)?
            .filter(|provisioner| provisioner.enabled)
            .map(|provisioner| controller_proto::KubernetesLocalProvisioner {
                enabled: true,
                storage_class_name: provisioner.storage_class_name,
                volumes: provisioner
                    .volumes
                    .into_iter()
                    .map(|volume| controller_proto::KubernetesHostVolume {
                        name: volume.name,
                        node_id: volume.node_id,
                        storage_class: volume.storage_class,
                        size_bytes: volume.size_bytes,
                        fs_type: volume.fs_type,
                        mount_path: volume.mount_path,
                        role: volume.role,
                        volume_group: volume.volume_group,
                    })
                    .collect(),
                mode: provisioner.mode,
            });

        Ok(controller_proto::KubernetesCluster {
            name: row.name.clone(),
            generation: row.generation,
            spec: Some(controller_proto::KubernetesClusterSpec {
                cluster_cidr: row.cluster_cidr.clone(),
                service_cidr: row.service_cidr.clone(),
                cluster_dns: row.cluster_dns.clone(),
                api_server_endpoint: row.api_server_endpoint.clone(),
                cni: row.cni.clone(),
                cilium_config,
                control_plane_nodes,
                worker_nodes,
                high_availability: ha_config,
                local_provisioner,
                schedule_on_control_plane: config
                    .as_ref()
                    .map(|c| c.schedule_on_control_plane)
                    .unwrap_or(false),
            }),
            status: Some(controller_proto::KubernetesClusterStatus {
                phase: row.phase.clone(),
                message: row.status_message.clone(),
                ready_control_planes: row.ready_control_planes,
                ready_workers: displayed_ready_workers(
                    row.ready_workers,
                    row.ready_control_planes,
                    config
                        .as_ref()
                        .map(|c| c.schedule_on_control_plane)
                        .unwrap_or(false),
                ),
                last_updated: None,
                active_node_ids: vec![],
            }),
        })
    }
}

/// The primary API keeps the requested CNI. Every other cluster on the same
/// nodes uses the bridge CNI; a second Cilium agent would share BPF pins.
fn stored_cni(api_endpoint: &str, requested: &str) -> String {
    if crate::nixgen::kubernetes_listen_ports(api_endpoint).api_server == 6443 {
        let requested = requested.trim();
        if requested.is_empty() {
            "cilium".to_string()
        } else {
            requested.to_string()
        }
    } else {
        "bridge".to_string()
    }
}

fn displayed_ready_workers(
    dedicated_ready: i32,
    ready_control_planes: i32,
    schedule_on_control_plane: bool,
) -> i32 {
    if schedule_on_control_plane {
        dedicated_ready.saturating_add(ready_control_planes)
    } else {
        dedicated_ready
    }
}

fn internal_db(e: rusqlite::Error) -> Status {
    Status::internal(format!("database error: {e}"))
}

fn operator_role_kind_to_auth(kind: i32) -> Result<OperatorRole, Status> {
    let k = controller_proto::OperatorRoleKind::try_from(kind)
        .unwrap_or(controller_proto::OperatorRoleKind::Unspecified);
    match k {
        controller_proto::OperatorRoleKind::ReadOnly => Ok(OperatorRole::ReadOnly),
        controller_proto::OperatorRoleKind::BackupOperator => Ok(OperatorRole::BackupOperator),
        controller_proto::OperatorRoleKind::VmAdmin => Ok(OperatorRole::VmAdmin),
        controller_proto::OperatorRoleKind::ClusterAdmin => Ok(OperatorRole::ClusterAdmin),
        controller_proto::OperatorRoleKind::Unspecified => Err(Status::invalid_argument(
            "role must be read_only, vm_admin, or cluster_admin",
        )),
    }
}

fn operator_roles_to_proto_kinds(roles: &[String]) -> Vec<i32> {
    roles
        .iter()
        .filter_map(|s| {
            OperatorRole::from_db_str(s).map(|r| match r {
                OperatorRole::ReadOnly => controller_proto::OperatorRoleKind::ReadOnly as i32,
                OperatorRole::BackupOperator => {
                    controller_proto::OperatorRoleKind::BackupOperator as i32
                }
                OperatorRole::VmAdmin => controller_proto::OperatorRoleKind::VmAdmin as i32,
                OperatorRole::ClusterAdmin => {
                    controller_proto::OperatorRoleKind::ClusterAdmin as i32
                }
            })
        })
        .collect()
}

fn operator_row_to_proto(
    db: &Database,
    row: &OperatorRow,
) -> Result<controller_proto::Operator, rusqlite::Error> {
    let roles = db.list_operator_role_strings(&row.name)?;
    Ok(controller_proto::Operator {
        name: row.name.clone(),
        cert_serial: row.cert_serial.clone(),
        created_at: Some(prost_types::Timestamp {
            seconds: row.created_at,
            nanos: 0,
        }),
        updated_at: Some(prost_types::Timestamp {
            seconds: row.updated_at,
            nanos: 0,
        }),
        roles: operator_roles_to_proto_kinds(&roles),
    })
}

/// `prost_types::Timestamp` for a `time::OffsetDateTime`.
fn prost_timestamp(t: time::OffsetDateTime) -> prost_types::Timestamp {
    prost_types::Timestamp {
        seconds: t.unix_timestamp(),
        nanos: 0,
    }
}

/// Inventory status column for a `CertificateStatus` filter value.
/// `UNSPECIFIED` means "no filter", represented as the empty string.
fn cert_status_to_db_str(status: i32) -> &'static str {
    match controller_proto::CertificateStatus::try_from(status) {
        Ok(controller_proto::CertificateStatus::Active) => crate::db::CERT_STATUS_ACTIVE,
        Ok(controller_proto::CertificateStatus::Rotated) => crate::db::CERT_STATUS_ROTATED,
        Ok(controller_proto::CertificateStatus::Revoked) => crate::db::CERT_STATUS_REVOKED,
        _ => "",
    }
}

fn cert_status_from_db_str(status: &str) -> i32 {
    let value = match status {
        crate::db::CERT_STATUS_ACTIVE => controller_proto::CertificateStatus::Active,
        crate::db::CERT_STATUS_ROTATED => controller_proto::CertificateStatus::Rotated,
        crate::db::CERT_STATUS_REVOKED => controller_proto::CertificateStatus::Revoked,
        _ => controller_proto::CertificateStatus::Unspecified,
    };
    value as i32
}

fn cert_info_from_row(
    row: &crate::db::IssuedCertRow,
    now: time::OffsetDateTime,
) -> controller_proto::CertificateInfo {
    let not_after = crate::pki::parse_ts(&row.not_after);
    controller_proto::CertificateInfo {
        serial_hex: row.serial_hex.clone(),
        subject_cn: row.subject_cn.clone(),
        identity_kind: row.identity_kind.clone(),
        node_id: row.node_id.clone(),
        issuer_cn: row.issuer_cn.clone(),
        fingerprint_sha256: row.fingerprint_sha256.clone(),
        not_before: crate::pki::parse_ts(&row.not_before).map(prost_timestamp),
        not_after: not_after.map(prost_timestamp),
        issued_at: crate::pki::parse_ts(&row.issued_at).map(prost_timestamp),
        status: cert_status_from_db_str(&row.status),
        revocation_reason: row.revocation_reason.max(0),
        revoked_at: crate::pki::parse_ts(&row.revoked_at).map(prost_timestamp),
        days_until_expiry: not_after
            .map(|na| crate::pki::days_until(na, now))
            .unwrap_or(0),
    }
}

fn leaf_serial_hex(chain_pem: &str) -> Result<String, Status> {
    let first_end = chain_pem
        .find("-----END CERTIFICATE-----")
        .ok_or_else(|| Status::internal("malformed PEM chain"))?;
    let first_block = chain_pem[..first_end + "-----END CERTIFICATE-----".len()].to_string();
    let pem = pem::parse(first_block).map_err(|e| Status::internal(format!("PEM parse: {e}")))?;
    use x509_parser::prelude::FromDer;
    let (_, cert) = x509_parser::certificate::X509Certificate::from_der(pem.contents())
        .map_err(|e| Status::internal(format!("X509 parse: {e}")))?;
    Ok(format!("{:X}", cert.serial))
}

#[cfg(test)]
#[allow(clippy::result_large_err)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;
    use crate::config::ReplicationConfig;

    fn empty_sub_ca() -> Arc<Mutex<SubCaState>> {
        Arc::new(Mutex::new(SubCaState::default()))
    }

    fn test_network() -> NetworkConfig {
        NetworkConfig {
            gateway_interface: "eno1".to_string(),
            external_ip: "203.0.113.10".to_string(),
            gateway_ip: "10.0.0.1".to_string(),
            internal_netmask: "255.255.255.0".to_string(),
        }
    }

    fn test_node() -> NodeRow {
        NodeRow {
            id: "node-1".to_string(),
            hostname: "node-1".to_string(),
            address: "127.0.0.1:59991".to_string(),
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
            image_url: "https://example.com/web-1.raw".to_string(),
            image_sha256: "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
                .to_string(),
            image_format: "raw".to_string(),
            image_size: 8192,
            network: "default".to_string(),
            auto_start: true,
            node_id: node_id.to_string(),
            created_at: String::new(),
            runtime_state: "unknown".to_string(),
            cloud_init_user_data: String::new(),
            storage_backend: "filesystem".to_string(),
            storage_size_bytes: 10 * 1024 * 1024 * 1024,
            vm_ip: String::new(),
            pci_devices: String::new(),
            labels: String::new(),
        }
    }

    #[tokio::test]
    async fn set_vm_desired_state_updates_db_and_invokes_push_hook() {
        let db = Database::open(":memory:").expect("open db");
        let node = test_node();
        db.upsert_node(&node).expect("insert node");
        db.insert_vm(&test_vm(&node.id)).expect("insert vm");

        let push_count = Arc::new(AtomicUsize::new(0));
        let pushed_node = Arc::new(Mutex::new(String::new()));
        let count_clone = Arc::clone(&push_count);
        let node_clone = Arc::clone(&pushed_node);
        let hook: PushHook = Arc::new(move |n: &NodeRow| {
            count_clone.fetch_add(1, Ordering::SeqCst);
            *node_clone.lock().expect("lock pushed node") = n.id.clone();
            Ok(())
        });

        let svc = ControllerService::new_with_test_push_hook(
            db.clone(),
            NodeClients::new(None),
            test_network(),
            None,
            false,
            hook,
        );

        let req = controller_proto::SetVmDesiredStateRequest {
            vm_id: "vm-1".to_string(),
            desired_state: controller_proto::VmDesiredState::Stopped as i32,
            target_node: node.id.clone(),
        };

        let resp = <ControllerService as controller_proto::controller_server::Controller>::set_vm_desired_state(
            &svc,
            Request::new(req),
        )
        .await
        .expect("set desired state")
        .into_inner();

        assert_eq!(resp.state, controller_proto::VmState::Stopped as i32);
        let vm = db.get_vm("vm-1").expect("get vm").expect("vm exists");
        assert!(
            !vm.auto_start,
            "desired stopped state should set auto_start=false"
        );
        assert_eq!(push_count.load(Ordering::SeqCst), 1);
        assert_eq!(*pushed_node.lock().expect("lock pushed node"), "node-1");
    }

    #[tokio::test]
    async fn set_vm_desired_state_rejects_unspecified_without_push() {
        let db = Database::open(":memory:").expect("open db");
        let node = test_node();
        db.upsert_node(&node).expect("insert node");
        db.insert_vm(&test_vm(&node.id)).expect("insert vm");

        let push_count = Arc::new(AtomicUsize::new(0));
        let count_clone = Arc::clone(&push_count);
        let hook: PushHook = Arc::new(move |_n: &NodeRow| {
            count_clone.fetch_add(1, Ordering::SeqCst);
            Ok(())
        });

        let svc = ControllerService::new_with_test_push_hook(
            db.clone(),
            NodeClients::new(None),
            test_network(),
            None,
            false,
            hook,
        );

        let req = controller_proto::SetVmDesiredStateRequest {
            vm_id: "vm-1".to_string(),
            desired_state: controller_proto::VmDesiredState::Unspecified as i32,
            target_node: String::new(),
        };

        let err = <ControllerService as controller_proto::controller_server::Controller>::set_vm_desired_state(
            &svc,
            Request::new(req),
        )
        .await
        .expect_err("unspecified should fail");

        assert_eq!(err.code(), tonic::Code::InvalidArgument);
        let vm = db.get_vm("vm-1").expect("get vm").expect("vm exists");
        assert!(
            vm.auto_start,
            "invalid request should not mutate desired state"
        );
        assert_eq!(push_count.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn create_ssh_key_appends_audit_event_and_list_returns_it() {
        let db = Database::open(":memory:").expect("open db");
        let svc = ControllerService::new(
            db.clone(),
            NodeClients::new(None),
            test_network(),
            empty_sub_ca(),
            None,
            false,
            false,
        );

        let resp =
            <ControllerService as controller_proto::controller_server::Controller>::create_ssh_key(
                &svc,
                Request::new(controller_proto::CreateSshKeyRequest {
                    name: "ops".into(),
                    public_key: "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAItestkey ops@kcore".into(),
                }),
            )
            .await
            .expect("create ssh key")
            .into_inner();
        assert!(resp.success);

        let listed =
            <ControllerService as controller_proto::controller_server::Controller>::list_audit_events(
                &svc,
                Request::new(controller_proto::ListAuditEventsRequest {
                    limit: 10,
                    since: String::new(),
                    action: "CreateSshKey".into(),
                }),
            )
            .await
            .expect("list audit events")
            .into_inner();

        assert_eq!(listed.events.len(), 1);
        let ev = &listed.events[0];
        assert_eq!(ev.actor, "insecure");
        assert_eq!(ev.action, "CreateSshKey");
        assert_eq!(ev.resource, "ssh-key/ops");
    }

    #[tokio::test]
    async fn console_session_open_audit_lists_actor_vm_and_time() {
        let db = Database::open(":memory:").expect("open db");
        let svc = ControllerService::new(
            db.clone(),
            NodeClients::new(None),
            test_network(),
            empty_sub_ca(),
            None,
            false,
            false,
        );

        svc.record_audit(
            "kctl:alice",
            "AttachVmConsole",
            "vm/web-01",
            r#"{"nodeId":"node-a"}"#,
        );

        let listed =
            <ControllerService as controller_proto::controller_server::Controller>::list_audit_events(
                &svc,
                Request::new(controller_proto::ListAuditEventsRequest {
                    limit: 10,
                    since: String::new(),
                    action: "AttachVmConsole".into(),
                }),
            )
            .await
            .expect("list audit events")
            .into_inner();

        assert_eq!(listed.events.len(), 1);
        let ev = &listed.events[0];
        assert_eq!(ev.actor, "kctl:alice");
        assert_eq!(ev.action, "AttachVmConsole");
        assert_eq!(ev.resource, "vm/web-01");
        assert!(
            !ev.created_at.is_empty(),
            "session open must record a timestamp"
        );
        assert!(ev.detail.contains("node-a"));
    }

    #[test]
    fn validate_image_url_requires_https() {
        let err = validate_image_url("http://example.com/debian.raw").expect_err("must fail");
        assert_eq!(err.code(), tonic::Code::InvalidArgument);
    }

    #[test]
    fn validate_image_sha256_requires_hex_len_64() {
        let err = validate_image_sha256("1234").expect_err("must fail");
        assert_eq!(err.code(), tonic::Code::InvalidArgument);
    }

    #[test]
    fn runtime_state_mapping_never_assumes_running() {
        assert_eq!(
            state_fallback_without_runtime(true),
            controller_proto::VmState::Unknown as i32
        );
        assert_eq!(
            state_fallback_without_runtime(false),
            controller_proto::VmState::Stopped as i32
        );
        assert_eq!(
            controller_state_from_node_state(crate::node_proto::VmState::Running as i32),
            controller_proto::VmState::Running as i32
        );
        assert_eq!(
            controller_state_from_node_state(crate::node_proto::VmState::Unknown as i32),
            controller_proto::VmState::Unknown as i32
        );
    }

    #[test]
    fn derive_local_image_path_is_deterministic() {
        let p1 = derive_local_image_path(
            "https://cloud.debian.org/images/cloud/bookworm/latest/debian-12-generic-amd64.qcow2",
            "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
        );
        let p2 = derive_local_image_path(
            "https://cloud.debian.org/images/cloud/bookworm/latest/debian-12-generic-amd64.qcow2",
            "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
        );
        assert_eq!(p1, p2);
        assert!(p1.starts_with("/var/lib/kcore/images/aaaaaaaaaaaa-"));
    }

    #[test]
    fn derive_image_format_uses_qcow2_extension() {
        assert_eq!(
            derive_image_format("https://example.com/debian-12-genericcloud-amd64.qcow2"),
            "qcow2"
        );
        assert_eq!(derive_image_format("https://example.com/rootfs.raw"), "raw");
    }

    #[test]
    fn validate_network_inputs_reject_bad_values() {
        let reserved = validate_network_name("default").expect_err("default is reserved");
        assert_eq!(reserved.code(), tonic::Code::InvalidArgument);
        let invalid_ip = validate_ipv4("10.0.0", "gateway_ip").expect_err("invalid ip");
        assert_eq!(invalid_ip.code(), tonic::Code::InvalidArgument);
        let invalid_mask =
            validate_netmask("255.0.255.0").expect_err("non-contiguous mask should fail");
        assert_eq!(invalid_mask.code(), tonic::Code::InvalidArgument);
    }

    #[tokio::test]
    async fn create_vm_rejects_missing_image_url_and_sha() {
        let db = Database::open(":memory:").expect("open db");
        let node = test_node();
        db.upsert_node(&node).expect("insert node");

        let hook: PushHook = Arc::new(|_n: &NodeRow| Ok(()));
        let svc = ControllerService::new_with_test_push_hook(
            db,
            NodeClients::new(None),
            test_network(),
            None,
            false,
            hook,
        );

        let req = controller_proto::CreateVmRequest {
            target_node: node.id,
            spec: Some(controller_proto::VmSpec {
                id: String::new(),
                name: "vm-a".to_string(),
                cpu: 1,
                memory_bytes: 512 * 1024 * 1024,
                disks: vec![],
                nics: vec![],
                storage_backend: String::new(),
                storage_size_bytes: 0,
                desired_state: controller_proto::VmDesiredState::Unspecified as i32,
                pci_devices: vec![],
                gpus: vec![],
                labels: std::collections::HashMap::new(),
                vgpus: vec![],
            }),
            image_url: String::new(),
            image_sha256: String::new(),
            cloud_init_user_data: String::new(),
            image_path: String::new(),
            image_format: String::new(),
            ssh_key_names: vec![],
            storage_backend: controller_proto::StorageBackendType::Filesystem as i32,
            storage_size_bytes: 8 * 1024 * 1024 * 1024,
            target_dc: String::new(),
            node_labels: Vec::new(),
            anti_affinity: String::new(),
            ..Default::default()
        };

        let err =
            <ControllerService as controller_proto::controller_server::Controller>::create_vm(
                &svc,
                Request::new(req),
            )
            .await
            .expect_err("missing image_url should be rejected");
        assert_eq!(err.code(), tonic::Code::InvalidArgument);
        assert!(err.message().contains("image_url"));
    }

    #[tokio::test]
    async fn create_vm_rolls_back_when_push_fails() {
        let db = Database::open(":memory:").expect("open db");
        let node = test_node();
        db.upsert_node(&node).expect("insert node");

        let hook: PushHook =
            Arc::new(|_n: &NodeRow| Err(Status::internal("simulated push failure for test")));
        let svc = ControllerService::new_with_test_push_hook(
            db.clone(),
            NodeClients::new(None),
            test_network(),
            None,
            false,
            hook,
        );

        let req = controller_proto::CreateVmRequest {
            target_node: node.id.clone(),
            spec: Some(controller_proto::VmSpec {
                id: String::new(),
                name: "vm-rollback".to_string(),
                cpu: 1,
                memory_bytes: 512 * 1024 * 1024,
                disks: vec![],
                nics: vec![],
                storage_backend: String::new(),
                storage_size_bytes: 0,
                desired_state: controller_proto::VmDesiredState::Unspecified as i32,
                pci_devices: vec![],
                gpus: vec![],
                labels: std::collections::HashMap::new(),
                vgpus: vec![],
            }),
            image_url: "https://example.com/debian.raw".to_string(),
            image_sha256: "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
                .to_string(),
            cloud_init_user_data: String::new(),
            image_path: String::new(),
            image_format: String::new(),
            ssh_key_names: vec![],
            storage_backend: controller_proto::StorageBackendType::Filesystem as i32,
            storage_size_bytes: 8 * 1024 * 1024 * 1024,
            target_dc: String::new(),
            node_labels: Vec::new(),
            anti_affinity: String::new(),
            ..Default::default()
        };

        let err =
            <ControllerService as controller_proto::controller_server::Controller>::create_vm(
                &svc,
                Request::new(req),
            )
            .await
            .expect_err("create should fail when push fails");
        assert_eq!(err.code(), tonic::Code::Aborted);

        let found = db
            .find_node_for_vm("vm-rollback")
            .expect("query vm by name after failed create");
        assert!(found.is_none(), "failed create should be rolled back");
    }

    #[tokio::test]
    async fn create_vm_rejects_image_path_already_in_use() {
        let db = Database::open(":memory:").expect("open db");
        let node = test_node();
        db.upsert_node(&node).expect("insert node");
        db.insert_vm(&test_vm(&node.id))
            .expect("insert existing vm");

        let hook: PushHook = Arc::new(|_n: &NodeRow| Ok(()));
        let svc = ControllerService::new_with_test_push_hook(
            db,
            NodeClients::new(None),
            test_network(),
            None,
            false,
            hook,
        );

        let req = controller_proto::CreateVmRequest {
            target_node: node.id,
            spec: Some(controller_proto::VmSpec {
                id: String::new(),
                name: "vm-path-conflict".to_string(),
                cpu: 1,
                memory_bytes: 512 * 1024 * 1024,
                disks: vec![],
                nics: vec![],
                storage_backend: String::new(),
                storage_size_bytes: 0,
                desired_state: controller_proto::VmDesiredState::Unspecified as i32,
                pci_devices: vec![],
                gpus: vec![],
                labels: std::collections::HashMap::new(),
                vgpus: vec![],
            }),
            image_url: String::new(),
            image_sha256: String::new(),
            cloud_init_user_data: String::new(),
            image_path: "/var/lib/kcore/images/web-1.raw".to_string(),
            image_format: "raw".to_string(),
            ssh_key_names: vec![],
            storage_backend: controller_proto::StorageBackendType::Filesystem as i32,
            storage_size_bytes: 8 * 1024 * 1024 * 1024,
            target_dc: String::new(),
            node_labels: Vec::new(),
            anti_affinity: String::new(),
            ..Default::default()
        };

        let err =
            <ControllerService as controller_proto::controller_server::Controller>::create_vm(
                &svc,
                Request::new(req),
            )
            .await
            .expect_err("duplicate image path should be rejected");
        assert_eq!(err.code(), tonic::Code::FailedPrecondition);
        assert!(err.message().contains("already used"));
    }

    #[tokio::test]
    async fn create_vm_rejects_storage_backend_mismatch() {
        let db = Database::open(":memory:").expect("open db");
        let mut node = test_node();
        node.storage_backend = "zfs".to_string();
        db.upsert_node(&node).expect("insert node");

        let hook: PushHook = Arc::new(|_n: &NodeRow| Ok(()));
        let svc = ControllerService::new_with_test_push_hook(
            db,
            NodeClients::new(None),
            test_network(),
            None,
            false,
            hook,
        );

        let req = controller_proto::CreateVmRequest {
            target_node: node.id,
            spec: Some(controller_proto::VmSpec {
                id: String::new(),
                name: "vm-storage-mismatch".to_string(),
                cpu: 1,
                memory_bytes: 512 * 1024 * 1024,
                disks: vec![],
                nics: vec![],
                storage_backend: String::new(),
                storage_size_bytes: 0,
                desired_state: controller_proto::VmDesiredState::Unspecified as i32,
                pci_devices: vec![],
                gpus: vec![],
                labels: std::collections::HashMap::new(),
                vgpus: vec![],
            }),
            image_url: String::new(),
            image_sha256: String::new(),
            cloud_init_user_data: String::new(),
            image_path: "/var/lib/kcore/images/base.raw".to_string(),
            image_format: "raw".to_string(),
            ssh_key_names: vec![],
            storage_backend: controller_proto::StorageBackendType::Filesystem as i32,
            storage_size_bytes: 8 * 1024 * 1024 * 1024,
            target_dc: String::new(),
            node_labels: Vec::new(),
            anti_affinity: String::new(),
            ..Default::default()
        };

        let err =
            <ControllerService as controller_proto::controller_server::Controller>::create_vm(
                &svc,
                Request::new(req),
            )
            .await
            .expect_err("mismatched storage backend should fail");
        assert_eq!(err.code(), tonic::Code::FailedPrecondition);
        assert!(err.message().contains("does not match"));
    }

    #[tokio::test]
    async fn create_vm_storage_backend_mismatch_auto_falls_back_when_possible() {
        let db = Database::open(":memory:").expect("open db");
        let mut wrong_node = test_node();
        wrong_node.id = "node-fs".to_string();
        wrong_node.storage_backend = "fs".to_string();
        db.upsert_node(&wrong_node).expect("insert wrong node");

        let mut candidate = test_node();
        candidate.id = "node-zfs".to_string();
        candidate.address = "127.0.0.2:59991".to_string();
        candidate.storage_backend = "zfs".to_string();
        db.upsert_node(&candidate).expect("insert candidate");

        let hook: PushHook = Arc::new(|_n: &NodeRow| Ok(()));
        let svc = ControllerService::new_with_test_push_hook(
            db,
            NodeClients::new(None),
            test_network(),
            None,
            false,
            hook,
        );

        let req = controller_proto::CreateVmRequest {
            target_node: wrong_node.id.clone(),
            spec: Some(controller_proto::VmSpec {
                id: String::new(),
                name: "vm-zfs-fallback".to_string(),
                cpu: 1,
                memory_bytes: 512 * 1024 * 1024,
                disks: vec![],
                nics: vec![],
                storage_backend: String::new(),
                storage_size_bytes: 0,
                desired_state: controller_proto::VmDesiredState::Unspecified as i32,
                pci_devices: vec![],
                gpus: vec![],
                labels: std::collections::HashMap::new(),
                vgpus: vec![],
            }),
            image_url: String::new(),
            image_sha256: String::new(),
            cloud_init_user_data: String::new(),
            image_path: "/var/lib/kcore/images/base.raw".to_string(),
            image_format: "raw".to_string(),
            ssh_key_names: vec![],
            storage_backend: controller_proto::StorageBackendType::Zfs as i32,
            storage_size_bytes: 8 * 1024 * 1024 * 1024,
            target_dc: String::new(),
            node_labels: Vec::new(),
            anti_affinity: String::new(),
            ..Default::default()
        };

        let resp =
            <ControllerService as controller_proto::controller_server::Controller>::create_vm(
                &svc,
                Request::new(req),
            )
            .await
            .expect("fallback should choose compatible node")
            .into_inner();
        assert_eq!(resp.node_id, "node-zfs");
    }

    #[tokio::test]
    async fn create_vm_rejects_target_node_without_capacity_in_preflight() {
        let db = Database::open(":memory:").expect("open db");
        let mut node = test_node();
        node.cpu_used = 4;
        node.memory_used = 8 * 1024 * 1024 * 1024;
        db.upsert_node(&node).expect("insert node");

        let hook: PushHook = Arc::new(|_n: &NodeRow| Ok(()));
        let svc = ControllerService::new_with_test_push_hook(
            db,
            NodeClients::new(None),
            test_network(),
            None,
            false,
            hook,
        );

        let req = controller_proto::CreateVmRequest {
            target_node: node.id,
            spec: Some(controller_proto::VmSpec {
                id: String::new(),
                name: "vm-no-capacity".to_string(),
                cpu: 1,
                memory_bytes: 512 * 1024 * 1024,
                disks: vec![],
                nics: vec![],
                storage_backend: String::new(),
                storage_size_bytes: 0,
                desired_state: controller_proto::VmDesiredState::Unspecified as i32,
                pci_devices: vec![],
                gpus: vec![],
                labels: std::collections::HashMap::new(),
                vgpus: vec![],
            }),
            image_url: String::new(),
            image_sha256: String::new(),
            cloud_init_user_data: String::new(),
            image_path: "/var/lib/kcore/images/base.raw".to_string(),
            image_format: "raw".to_string(),
            ssh_key_names: vec![],
            storage_backend: controller_proto::StorageBackendType::Filesystem as i32,
            storage_size_bytes: 8 * 1024 * 1024 * 1024,
            target_dc: String::new(),
            node_labels: Vec::new(),
            anti_affinity: String::new(),
            ..Default::default()
        };

        let err =
            <ControllerService as controller_proto::controller_server::Controller>::create_vm(
                &svc,
                Request::new(req),
            )
            .await
            .expect_err("preflight capacity check should fail");
        assert_eq!(err.code(), tonic::Code::Unavailable);
        assert!(err.message().contains("lacks capacity"));
    }

    #[tokio::test]
    async fn create_vm_preflight_auto_falls_back_to_alternative_node() {
        let db = Database::open(":memory:").expect("open db");
        let mut overloaded = test_node();
        overloaded.id = "node-overloaded".to_string();
        overloaded.cpu_used = 4;
        overloaded.memory_used = 8 * 1024 * 1024 * 1024;
        db.upsert_node(&overloaded).expect("insert overloaded node");

        let mut candidate = test_node();
        candidate.id = "node-candidate".to_string();
        candidate.address = "127.0.0.2:59991".to_string();
        db.upsert_node(&candidate).expect("insert candidate node");

        let hook: PushHook = Arc::new(|_n: &NodeRow| Ok(()));
        let svc = ControllerService::new_with_test_push_hook(
            db,
            NodeClients::new(None),
            test_network(),
            None,
            false,
            hook,
        );

        let req = controller_proto::CreateVmRequest {
            target_node: "node-overloaded".to_string(),
            spec: Some(controller_proto::VmSpec {
                id: String::new(),
                name: "vm-hint".to_string(),
                cpu: 1,
                memory_bytes: 512 * 1024 * 1024,
                disks: vec![],
                nics: vec![],
                storage_backend: String::new(),
                storage_size_bytes: 0,
                desired_state: controller_proto::VmDesiredState::Unspecified as i32,
                pci_devices: vec![],
                gpus: vec![],
                labels: std::collections::HashMap::new(),
                vgpus: vec![],
            }),
            image_url: String::new(),
            image_sha256: String::new(),
            cloud_init_user_data: String::new(),
            image_path: "/var/lib/kcore/images/base.raw".to_string(),
            image_format: "raw".to_string(),
            ssh_key_names: vec![],
            storage_backend: controller_proto::StorageBackendType::Filesystem as i32,
            storage_size_bytes: 8 * 1024 * 1024 * 1024,
            target_dc: String::new(),
            node_labels: Vec::new(),
            anti_affinity: String::new(),
            ..Default::default()
        };

        let resp =
            <ControllerService as controller_proto::controller_server::Controller>::create_vm(
                &svc,
                Request::new(req),
            )
            .await
            .expect("preflight should auto-fallback to alternative")
            .into_inner();
        assert_eq!(resp.node_id, "node-candidate");
    }

    #[tokio::test]
    async fn create_vm_rejects_target_node_not_ready_in_preflight() {
        let db = Database::open(":memory:").expect("open db");
        let mut node = test_node();
        node.status = "not-ready".to_string();
        db.upsert_node(&node).expect("insert node");

        let hook: PushHook = Arc::new(|_n: &NodeRow| Ok(()));
        let svc = ControllerService::new_with_test_push_hook(
            db,
            NodeClients::new(None),
            test_network(),
            None,
            false,
            hook,
        );

        let req = controller_proto::CreateVmRequest {
            target_node: node.id,
            spec: Some(controller_proto::VmSpec {
                id: String::new(),
                name: "vm-node-not-ready".to_string(),
                cpu: 1,
                memory_bytes: 512 * 1024 * 1024,
                disks: vec![],
                nics: vec![],
                storage_backend: String::new(),
                storage_size_bytes: 0,
                desired_state: controller_proto::VmDesiredState::Unspecified as i32,
                pci_devices: vec![],
                gpus: vec![],
                labels: std::collections::HashMap::new(),
                vgpus: vec![],
            }),
            image_url: String::new(),
            image_sha256: String::new(),
            cloud_init_user_data: String::new(),
            image_path: "/var/lib/kcore/images/base.raw".to_string(),
            image_format: "raw".to_string(),
            ssh_key_names: vec![],
            storage_backend: controller_proto::StorageBackendType::Filesystem as i32,
            storage_size_bytes: 8 * 1024 * 1024 * 1024,
            target_dc: String::new(),
            node_labels: Vec::new(),
            anti_affinity: String::new(),
            ..Default::default()
        };

        let err =
            <ControllerService as controller_proto::controller_server::Controller>::create_vm(
                &svc,
                Request::new(req),
            )
            .await
            .expect_err("preflight readiness check should fail");
        assert_eq!(err.code(), tonic::Code::Unavailable);
        assert!(err.message().contains("not ready"));
    }

    #[tokio::test]
    async fn drain_node_moves_vms_to_target_and_pushes_config() {
        let db = Database::open(":memory:").expect("open db");

        let mut node_a = test_node();
        node_a.id = "node-a".to_string();
        node_a.hostname = "node-a".to_string();
        db.upsert_node(&node_a).expect("insert node-a");

        let mut node_b = test_node();
        node_b.id = "node-b".to_string();
        node_b.hostname = "node-b".to_string();
        node_b.address = "127.0.0.2:59991".to_string();
        db.upsert_node(&node_b).expect("insert node-b");

        let mut vm1 = test_vm("node-a");
        vm1.id = "vm-drain-1".to_string();
        vm1.name = "drain-web-1".to_string();
        db.insert_vm(&vm1).expect("insert vm1");

        let mut vm2 = test_vm("node-a");
        vm2.id = "vm-drain-2".to_string();
        vm2.name = "drain-web-2".to_string();
        db.insert_vm(&vm2).expect("insert vm2");

        let pushed_nodes: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let pushed_clone = Arc::clone(&pushed_nodes);
        let hook: PushHook = Arc::new(move |n: &NodeRow| {
            pushed_clone.lock().expect("lock").push(n.id.clone());
            Ok(())
        });

        let svc = ControllerService::new_with_test_push_hook(
            db.clone(),
            NodeClients::new(None),
            test_network(),
            None,
            false,
            hook,
        );

        let resp =
            <ControllerService as controller_proto::controller_server::Controller>::drain_node(
                &svc,
                Request::new(controller_proto::DrainNodeRequest {
                    node_id: "node-a".to_string(),
                    target_node: "node-b".to_string(),
                }),
            )
            .await
            .expect("drain should succeed")
            .into_inner();

        assert!(resp.success, "drain should succeed: {}", resp.message);
        assert_eq!(resp.vms_migrated, 2);

        let node_a_vms = db.list_vms_for_node("node-a").expect("list vms node-a");
        assert!(
            node_a_vms.is_empty(),
            "node-a should have no VMs after drain"
        );

        let node_b_vms = db.list_vms_for_node("node-b").expect("list vms node-b");
        assert_eq!(node_b_vms.len(), 2, "node-b should have 2 VMs after drain");

        let pushed = pushed_nodes.lock().expect("lock");
        assert!(
            pushed.contains(&"node-a".to_string()),
            "should push config to drained node: {:?}",
            *pushed
        );
        assert!(
            pushed.contains(&"node-b".to_string()),
            "should push config to target node: {:?}",
            *pushed
        );

        let node_a_status = db
            .get_node("node-a")
            .expect("get node-a")
            .expect("node-a exists");
        assert_eq!(node_a_status.status, "drained");
    }

    #[tokio::test]
    async fn migrate_vm_rejects_non_ceph_backend() {
        let db = Database::open(":memory:").expect("open db");
        let mut node_a = test_node();
        node_a.id = "node-a".into();
        db.upsert_node(&node_a).unwrap();
        let mut node_b = test_node();
        node_b.id = "node-b".into();
        node_b.address = "127.0.0.2:59991".into();
        db.upsert_node(&node_b).unwrap();
        let mut vm = test_vm("node-a");
        vm.storage_backend = "filesystem".into();
        db.insert_vm(&vm).unwrap();
        let hook: PushHook = Arc::new(|_: &NodeRow| Ok(()));
        let svc = ControllerService::new_with_test_push_hook(
            db,
            NodeClients::new(None),
            test_network(),
            None,
            false,
            hook,
        );
        let err =
            <ControllerService as controller_proto::controller_server::Controller>::migrate_vm(
                &svc,
                Request::new(controller_proto::MigrateVmRequest {
                    vm_id: vm.id,
                    target_node: "node-b".into(),
                    allow_cold_fallback: false,
                    gpus: vec![],
                    vgpus: vec![],
                    ..Default::default()
                }),
            )
            .await
            .expect_err("non-ceph should fail");
        assert_eq!(err.code(), tonic::Code::FailedPrecondition);
        assert!(
            err.message().contains("filesystem"),
            "filesystem refusal should name the backend: {}",
            err.message()
        );
    }

    #[tokio::test]
    async fn migrate_vm_rejects_lvm_on_a_filesystem_node_without_dialing() {
        let db = Database::open(":memory:").expect("open db");
        let mut node_a = test_node();
        node_a.id = "node-a".into();
        db.upsert_node(&node_a).unwrap();
        let mut node_b = test_node();
        node_b.id = "node-b".into();
        node_b.address = "127.0.0.2:59991".into();
        node_b.storage_backend = "lvm".into();
        db.upsert_node(&node_b).unwrap();
        let mut vm = test_vm("node-a");
        vm.storage_backend = "lvm".into();
        db.insert_vm(&vm).unwrap();
        let hook: PushHook = Arc::new(|_: &NodeRow| Ok(()));
        let svc = ControllerService::new_with_test_push_hook(
            db,
            NodeClients::new(None),
            test_network(),
            None,
            false,
            hook,
        );
        let err =
            <ControllerService as controller_proto::controller_server::Controller>::migrate_vm(
                &svc,
                Request::new(controller_proto::MigrateVmRequest {
                    vm_id: vm.id,
                    target_node: "node-b".into(),
                    allow_cold_fallback: true,
                    gpus: vec![],
                    vgpus: vec![],
                    ..Default::default()
                }),
            )
            .await
            .expect_err("lvm VM on a filesystem node should fail before any RPC");
        assert_eq!(err.code(), tonic::Code::FailedPrecondition);
        assert!(
            err.message().contains("node-a"),
            "refusal should name the source: {}",
            err.message()
        );
    }

    #[tokio::test]
    async fn migrate_vm_rejects_pci_passthrough() {
        let db = Database::open(":memory:").expect("open db");
        let mut node_a = test_node();
        node_a.id = "node-a".into();
        db.upsert_node(&node_a).unwrap();
        let mut node_b = test_node();
        node_b.id = "node-b".into();
        node_b.address = "127.0.0.2:59991".into();
        db.upsert_node(&node_b).unwrap();
        let mut vm = test_vm("node-a");
        vm.pci_devices = "0000:03:00.0,0000:03:00.1".into();
        db.insert_vm(&vm).unwrap();
        let hook: PushHook = Arc::new(|_: &NodeRow| Ok(()));
        let svc = ControllerService::new_with_test_push_hook(
            db.clone(),
            NodeClients::new(None),
            test_network(),
            None,
            false,
            hook,
        );
        let err =
            <ControllerService as controller_proto::controller_server::Controller>::migrate_vm(
                &svc,
                Request::new(controller_proto::MigrateVmRequest {
                    vm_id: vm.id.clone(),
                    target_node: "node-b".into(),
                    allow_cold_fallback: true,
                    gpus: vec![],
                    vgpus: vec![],
                    ..Default::default()
                }),
            )
            .await
            .expect_err("pci vm should not migrate");
        assert_eq!(err.code(), tonic::Code::FailedPrecondition);
        assert!(err.message().contains("PCI"));
        let stayed = db.get_vm(&vm.id).unwrap().unwrap();
        assert_eq!(stayed.node_id, "node-a");
    }

    fn gpu_row(node: &str, name: &str, address: &str) -> crate::db::NodeGpuRow {
        crate::db::NodeGpuRow {
            node_id: node.into(),
            name: name.into(),
            family: "radeon".into(),
            model: "AMD 1002:744c".into(),
            address: address.into(),
            pci_devices: address.into(),
            iommu_group: 4,
            assignable: true,
            blocked_reason: String::new(),
            kind: "gpu".into(),
            role: "compute".into(),
            class_code: "0x030000".into(),
            characteristics: "purpose=ai".into(),
            driver: String::new(),
            parent: String::new(),
            profile: String::new(),
            profiles: String::new(),
        }
    }

    #[tokio::test]
    async fn migrate_vm_reassigns_a_free_gpu_on_the_destination() {
        let db = Database::open(":memory:").expect("open db");
        let mut node_a = test_node();
        node_a.id = "node-a".into();
        db.upsert_node(&node_a).unwrap();
        let mut node_b = test_node();
        node_b.id = "node-b".into();
        node_b.address = "127.0.0.2:59991".into();
        db.upsert_node(&node_b).unwrap();
        db.replace_node_gpus("node-a", &[gpu_row("node-a", "radeon0", "0000:03:00.0")])
            .unwrap();
        db.replace_node_gpus("node-b", &[gpu_row("node-b", "radeon0", "0000:41:00.0")])
            .unwrap();
        let mut vm = test_vm("node-a");
        vm.pci_devices = "0000:03:00.0".into();
        db.insert_vm(&vm).unwrap();
        db.set_vm_gpu(&vm.id, "radeon0").unwrap();
        let hook: PushHook = Arc::new(|_: &NodeRow| Ok(()));
        let svc = ControllerService::new_with_test_push_hook(
            db.clone(),
            NodeClients::new(None),
            test_network(),
            None,
            false,
            hook,
        );
        let resp =
            <ControllerService as controller_proto::controller_server::Controller>::migrate_vm(
                &svc,
                Request::new(controller_proto::MigrateVmRequest {
                    vm_id: vm.id.clone(),
                    target_node: "node-b".into(),
                    allow_cold_fallback: false,
                    gpus: vec![],
                    vgpus: vec![],
                    ..Default::default()
                }),
            )
            .await
            .expect("gpu vm should cold-migrate")
            .into_inner();
        assert!(resp.success, "{}", resp.message);
        assert_eq!(resp.mode, "cold");
        let moved = db.get_vm(&vm.id).unwrap().unwrap();
        assert_eq!(moved.node_id, "node-b");
        assert_eq!(moved.pci_devices, "0000:41:00.0");
        assert_eq!(db.get_vm_gpu(&vm.id).unwrap().as_deref(), Some("radeon0"));
    }

    fn vgpu_host(node: &str, name: &str, address: &str) -> crate::db::NodeGpuRow {
        let mut row = gpu_row(node, name, address);
        row.family = "nvidia".into();
        row.model = "NVIDIA 10de:25b6".into();
        row.role = "vgpu-host".into();
        row.assignable = false;
        row.blocked_reason = "vGPU host card; request a slice with --vgpu".into();
        row.driver = "nvidia".into();
        row
    }

    fn vgpu_slice(
        node: &str,
        name: &str,
        address: &str,
        parent: &str,
        profile: &str,
        profiles: &str,
    ) -> crate::db::NodeGpuRow {
        let mut row = gpu_row(node, name, address);
        row.family = "nvidia".into();
        row.model = "NVIDIA 10de:25b6 VF".into();
        row.kind = "vgpu".into();
        row.role = "slice".into();
        row.parent = parent.into();
        row.profile = profile.into();
        row.profiles = profiles.into();
        row.driver = "nvidia-vgpu-vfio".into();
        row
    }

    #[tokio::test]
    async fn migrate_vm_reassigns_a_free_vgpu_slice_of_the_same_profile() {
        let db = Database::open(":memory:").expect("open db");
        let mut node_a = test_node();
        node_a.id = "node-a".into();
        db.upsert_node(&node_a).unwrap();
        let mut node_b = test_node();
        node_b.id = "node-b".into();
        node_b.address = "127.0.0.2:59991".into();
        db.upsert_node(&node_b).unwrap();
        db.replace_node_gpus(
            "node-a",
            &[
                vgpu_host("node-a", "nvidia0", "0000:af:00.0"),
                vgpu_slice(
                    "node-a",
                    "nvidia0/0",
                    "0000:af:00.4",
                    "nvidia0",
                    "A2-4C",
                    "A2-4C,A2-8C",
                ),
            ],
        )
        .unwrap();
        db.replace_node_gpus(
            "node-b",
            &[
                vgpu_host("node-b", "nvidia0", "0000:c1:00.0"),
                vgpu_slice(
                    "node-b",
                    "nvidia0/0",
                    "0000:c1:00.4",
                    "nvidia0",
                    "",
                    "A2-4C,A2-8C",
                ),
            ],
        )
        .unwrap();
        let mut vm = test_vm("node-a");
        vm.pci_devices = "0000:af:00.4".into();
        db.insert_vm(&vm).unwrap();
        db.set_vm_gpu(&vm.id, "nvidia0/0=A2-4C").unwrap();
        let hook: PushHook = Arc::new(|_: &NodeRow| Ok(()));
        let svc = ControllerService::new_with_test_push_hook(
            db.clone(),
            NodeClients::new(None),
            test_network(),
            None,
            false,
            hook,
        );
        let resp =
            <ControllerService as controller_proto::controller_server::Controller>::migrate_vm(
                &svc,
                Request::new(controller_proto::MigrateVmRequest {
                    vm_id: vm.id.clone(),
                    target_node: "node-b".into(),
                    allow_cold_fallback: false,
                    gpus: vec![],
                    vgpus: vec![],
                    ..Default::default()
                }),
            )
            .await
            .expect("vgpu vm should cold-migrate")
            .into_inner();
        assert!(resp.success, "{}", resp.message);
        assert!(resp.message.contains("A2-4C"), "{}", resp.message);
        let moved = db.get_vm(&vm.id).unwrap().unwrap();
        assert_eq!(moved.node_id, "node-b");
        assert_eq!(moved.pci_devices, "0000:c1:00.4");
        assert_eq!(
            db.get_vm_gpu(&vm.id).unwrap().as_deref(),
            Some("nvidia0/0=A2-4C")
        );
    }

    #[tokio::test]
    async fn drain_leaves_a_vgpu_vm_when_no_node_has_its_profile() {
        let db = Database::open(":memory:").expect("open db");
        let mut node_a = test_node();
        node_a.id = "node-a".into();
        db.upsert_node(&node_a).unwrap();
        let mut node_b = test_node();
        node_b.id = "node-b".into();
        node_b.address = "127.0.0.2:59991".into();
        db.upsert_node(&node_b).unwrap();
        db.replace_node_gpus(
            "node-a",
            &[
                vgpu_host("node-a", "nvidia0", "0000:af:00.0"),
                vgpu_slice(
                    "node-a",
                    "nvidia0/0",
                    "0000:af:00.4",
                    "nvidia0",
                    "A2-4C",
                    "A2-4C",
                ),
            ],
        )
        .unwrap();
        db.replace_node_gpus(
            "node-b",
            &[
                vgpu_host("node-b", "nvidia0", "0000:c1:00.0"),
                vgpu_slice(
                    "node-b",
                    "nvidia0/0",
                    "0000:c1:00.4",
                    "nvidia0",
                    "A2-8C",
                    "A2-8C",
                ),
            ],
        )
        .unwrap();
        let mut vm = test_vm("node-a");
        vm.pci_devices = "0000:af:00.4".into();
        db.insert_vm(&vm).unwrap();
        db.set_vm_gpu(&vm.id, "nvidia0/0=A2-4C").unwrap();
        let hook: PushHook = Arc::new(|_: &NodeRow| Ok(()));
        let svc = ControllerService::new_with_test_push_hook(
            db.clone(),
            NodeClients::new(None),
            test_network(),
            None,
            false,
            hook,
        );
        let resp =
            <ControllerService as controller_proto::controller_server::Controller>::drain_node(
                &svc,
                Request::new(controller_proto::DrainNodeRequest {
                    node_id: "node-a".into(),
                    target_node: String::new(),
                }),
            )
            .await
            .expect("drain rpc")
            .into_inner();
        assert!(resp.message.contains("A2-4C"), "{}", resp.message);
        let stayed = db.get_vm(&vm.id).unwrap().unwrap();
        assert_eq!(stayed.node_id, "node-a");
        let node = db.get_node("node-a").unwrap().unwrap();
        assert_ne!(node.status, "drained");
    }

    #[tokio::test]
    async fn drain_node_leaves_pci_vm_in_place() {
        let db = Database::open(":memory:").expect("open db");
        let mut node_a = test_node();
        node_a.id = "node-a".into();
        db.upsert_node(&node_a).unwrap();
        let mut node_b = test_node();
        node_b.id = "node-b".into();
        node_b.address = "127.0.0.2:59991".into();
        db.upsert_node(&node_b).unwrap();

        let mut movable = test_vm("node-a");
        movable.id = "vm-plain".into();
        movable.name = "plain".into();
        db.insert_vm(&movable).unwrap();

        let mut pinned = test_vm("node-a");
        pinned.id = "vm-gpu".into();
        pinned.name = "gpu-guest".into();
        pinned.pci_devices = "0000:03:00.0".into();
        db.insert_vm(&pinned).unwrap();

        let hook: PushHook = Arc::new(|_: &NodeRow| Ok(()));
        let svc = ControllerService::new_with_test_push_hook(
            db.clone(),
            NodeClients::new(None),
            test_network(),
            None,
            false,
            hook,
        );
        let resp =
            <ControllerService as controller_proto::controller_server::Controller>::drain_node(
                &svc,
                Request::new(controller_proto::DrainNodeRequest {
                    node_id: "node-a".into(),
                    target_node: "node-b".into(),
                }),
            )
            .await
            .expect("drain returns")
            .into_inner();
        assert!(!resp.success, "{}", resp.message);
        assert!(resp.message.contains("gpu-guest"));
        assert_eq!(resp.vms_migrated, 1);
        let pinned_now = db.get_vm("vm-gpu").unwrap().unwrap();
        assert_eq!(pinned_now.node_id, "node-a");
        let moved = db.get_vm("vm-plain").unwrap().unwrap();
        assert_eq!(moved.node_id, "node-b");
        assert_eq!(
            db.get_node("node-a").unwrap().unwrap().status,
            "draining",
            "a pinned GPU VM means the node is not fully drained"
        );
    }

    #[tokio::test]
    async fn create_vm_rejects_overlapping_pci_device() {
        let db = Database::open(":memory:").expect("open db");
        let node = test_node();
        db.upsert_node(&node).unwrap();
        let mut existing = test_vm(&node.id);
        existing.id = "vm-gpu".into();
        existing.name = "gpu-a".into();
        existing.image_path = "/var/lib/kcore/images/gpu-a.raw".into();
        existing.pci_devices = "0000:03:00.0,0000:03:00.1".into();
        db.insert_vm(&existing).unwrap();

        let hook: PushHook = Arc::new(|_: &NodeRow| Ok(()));
        let svc = ControllerService::new_with_test_push_hook(
            db,
            NodeClients::new(None),
            test_network(),
            None,
            false,
            hook,
        );
        let req = controller_proto::CreateVmRequest {
            target_node: node.id,
            spec: Some(controller_proto::VmSpec {
                id: String::new(),
                name: "gpu-b".into(),
                cpu: 1,
                memory_bytes: 512 * 1024 * 1024,
                disks: vec![],
                nics: vec![],
                storage_backend: String::new(),
                storage_size_bytes: 0,
                desired_state: controller_proto::VmDesiredState::Unspecified as i32,
                pci_devices: vec!["03:00.1".into()],
                gpus: vec![],
                labels: std::collections::HashMap::new(),
                vgpus: vec![],
            }),
            image_url: String::new(),
            image_sha256: String::new(),
            cloud_init_user_data: String::new(),
            image_path: "/var/lib/kcore/images/gpu-b.raw".into(),
            image_format: "raw".into(),
            ssh_key_names: vec![],
            storage_backend: controller_proto::StorageBackendType::Filesystem as i32,
            storage_size_bytes: 8 * 1024 * 1024 * 1024,
            target_dc: String::new(),
            node_labels: Vec::new(),
            anti_affinity: String::new(),
            ..Default::default()
        };
        let err =
            <ControllerService as controller_proto::controller_server::Controller>::create_vm(
                &svc,
                Request::new(req),
            )
            .await
            .expect_err("overlapping pci should fail");
        assert_eq!(err.code(), tonic::Code::AlreadyExists);
        assert!(err.message().contains("0000:03:00.1"));
        assert!(err.message().contains("gpu-a"));
    }

    fn mark_ceph_cluster_healthy(db: &Database, name: &str) {
        db.upsert_ceph_cluster_status(&CephClusterStatusRow {
            name: name.to_string(),
            observed_generation: 1,
            phase: "healthy".into(),
            health_message: "HEALTH_OK".into(),
            ceph_status_json: String::new(),
            last_transition_at: String::new(),
        })
        .expect("upsert ceph status");
    }

    /// Builds two Ceph-capable nodes plus one Ceph-backed VM with a volume row
    /// on `node-a`. The CephCluster has no status row, so callers decide
    /// whether it counts as healthy.
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
            host_node_id: String::new(),
            fs_type: String::new(),
            mount_point: String::new(),
            created_at: String::new(),
        }
    }

    fn ceph_two_node_fixture() -> (Database, VmRow) {
        let db = Database::open(":memory:").expect("open db");
        let mut node_a = test_node();
        node_a.id = "node-a".into();
        db.upsert_node(&node_a).unwrap();
        let mut node_b = test_node();
        node_b.id = "node-b".into();
        node_b.address = "127.0.0.2:59991".into();
        db.upsert_node(&node_b).unwrap();

        let mut spec = make_ceph_spec(&["node-a", "node-b"]);
        spec.fsid = "abababab-cdcd-efef-0101-232323232323".into();
        db.upsert_ceph_cluster(&CephClusterRow {
            name: "lab".into(),
            generation: 1,
            spec_json: crate::ceph_cluster_spec::spec_to_json(&spec).unwrap(),
            bootstrap_json: String::new(),
            created_at: String::new(),
            updated_at: String::new(),
        })
        .unwrap();

        let mut vm = test_vm("node-a");
        vm.id = "vm-ceph-1".into();
        vm.name = "ceph-1".into();
        vm.storage_backend = "ceph".into();
        db.insert_vm(&vm).unwrap();
        db.upsert_volume(&test_root_volume(
            "vol-ceph-1",
            &vm.id,
            &vm.name,
            8 * 1024 * 1024 * 1024,
        ))
        .unwrap();
        (db, vm)
    }

    fn svc_for(db: &Database) -> ControllerService {
        let hook: PushHook = Arc::new(|_: &NodeRow| Ok(()));
        ControllerService::new_with_test_push_hook(
            db.clone(),
            NodeClients::new(None),
            test_network(),
            None,
            false,
            hook,
        )
    }

    #[tokio::test]
    async fn migrate_vm_rejects_target_outside_a_healthy_ceph_cluster() {
        let (db, vm) = ceph_two_node_fixture();
        // No status row at all: the cluster has never reconciled healthy.
        let svc = svc_for(&db);
        let err =
            <ControllerService as controller_proto::controller_server::Controller>::migrate_vm(
                &svc,
                Request::new(controller_proto::MigrateVmRequest {
                    vm_id: vm.id.clone(),
                    target_node: "node-b".into(),
                    allow_cold_fallback: true,
                    gpus: vec![],
                    vgpus: vec![],
                    ..Default::default()
                }),
            )
            .await
            .expect_err("unhealthy cluster must not accept a migration");
        assert_eq!(err.code(), tonic::Code::FailedPrecondition);
        assert!(
            err.message().contains("healthy CephCluster"),
            "unexpected message: {}",
            err.message()
        );
        assert_eq!(
            db.get_vm(&vm.id).unwrap().unwrap().node_id,
            "node-a",
            "a rejected migration must leave ownership alone"
        );
    }

    #[tokio::test]
    async fn migrate_vm_accepts_target_in_a_healthy_ceph_cluster() {
        let (db, vm) = ceph_two_node_fixture();
        mark_ceph_cluster_healthy(&db, "lab");
        let svc = svc_for(&db);
        let resp =
            <ControllerService as controller_proto::controller_server::Controller>::migrate_vm(
                &svc,
                Request::new(controller_proto::MigrateVmRequest {
                    vm_id: vm.id.clone(),
                    target_node: "node-b".into(),
                    allow_cold_fallback: true,
                    gpus: vec![],
                    vgpus: vec![],
                    ..Default::default()
                }),
            )
            .await
            .expect("healthy cluster should allow the cold fallback")
            .into_inner();
        assert_eq!(resp.mode, "cold");
        assert_eq!(db.get_vm(&vm.id).unwrap().unwrap().node_id, "node-b");
    }

    #[tokio::test]
    async fn drain_node_leaves_ceph_vm_when_no_healthy_cluster_target() {
        let (db, vm) = ceph_two_node_fixture();
        // node-b is a cluster member but the cluster is not healthy.
        let svc = svc_for(&db);
        let resp =
            <ControllerService as controller_proto::controller_server::Controller>::drain_node(
                &svc,
                Request::new(controller_proto::DrainNodeRequest {
                    node_id: "node-a".into(),
                    target_node: String::new(),
                }),
            )
            .await
            .expect("drain reports per-VM errors rather than failing outright")
            .into_inner();
        assert!(
            !resp.success,
            "drain must not claim success: {}",
            resp.message
        );
        assert_eq!(resp.vms_migrated, 0);
        assert_eq!(db.get_vm(&vm.id).unwrap().unwrap().node_id, "node-a");
        assert_eq!(
            db.get_node("node-a").unwrap().unwrap().status,
            "draining",
            "a node that still hosts VMs must not be marked drained"
        );
    }

    #[tokio::test]
    async fn drain_node_marks_drained_only_when_every_vm_moved() {
        let (db, vm) = ceph_two_node_fixture();
        mark_ceph_cluster_healthy(&db, "lab");
        let svc = svc_for(&db);
        let resp =
            <ControllerService as controller_proto::controller_server::Controller>::drain_node(
                &svc,
                Request::new(controller_proto::DrainNodeRequest {
                    node_id: "node-a".into(),
                    target_node: "node-b".into(),
                }),
            )
            .await
            .expect("drain should succeed")
            .into_inner();
        assert!(resp.success, "{}", resp.message);
        assert_eq!(resp.vms_migrated, 1);
        assert_eq!(db.get_vm(&vm.id).unwrap().unwrap().node_id, "node-b");
        assert_eq!(db.get_node("node-a").unwrap().unwrap().status, "drained");
    }

    /// Attach `web` to `vm_id` so a test can assert the attachment survives an
    /// operation that rewrites the VM's ownership.
    fn attach_web_security_group(db: &Database, vm_id: &str) {
        db.upsert_security_group(&SecurityGroupRow {
            name: "web".into(),
            description: "web ingress".into(),
            created_at: String::new(),
        })
        .expect("create security group");
        db.attach_security_group_to_vm("web", vm_id)
            .expect("attach security group");
    }

    fn attach_ops_ssh_key(db: &Database, vm_id: &str) {
        db.insert_ssh_key("ops", "ssh-ed25519 AAAAtest ops@kcore")
            .expect("create ssh key");
        db.associate_vm_ssh_keys(vm_id, &["ops".to_string()])
            .expect("associate ssh key");
    }

    /// Regression: reassignment used to delete and re-insert the `vms` row,
    /// which cascaded `security_group_vm_attachments` away. A migrated VM
    /// silently lost its firewall rules.
    #[tokio::test]
    async fn migrate_vm_preserves_security_group_attachments() {
        let (db, vm) = ceph_two_node_fixture();
        mark_ceph_cluster_healthy(&db, "lab");
        attach_web_security_group(&db, &vm.id);
        attach_ops_ssh_key(&db, &vm.id);

        let svc = svc_for(&db);
        let resp =
            <ControllerService as controller_proto::controller_server::Controller>::migrate_vm(
                &svc,
                Request::new(controller_proto::MigrateVmRequest {
                    vm_id: vm.id.clone(),
                    target_node: "node-b".into(),
                    allow_cold_fallback: true,
                    gpus: vec![],
                    vgpus: vec![],
                    ..Default::default()
                }),
            )
            .await
            .expect("cold fallback should succeed")
            .into_inner();
        assert!(resp.success, "{}", resp.message);
        assert_eq!(db.get_vm(&vm.id).unwrap().unwrap().node_id, "node-b");
        assert_eq!(
            db.list_security_groups_for_vm(&vm.id).unwrap(),
            vec!["web".to_string()],
            "a migrated VM must keep its security group attachments"
        );
        assert_eq!(
            db.get_vm_ssh_key_names(&vm.id).unwrap(),
            vec!["ops".to_string()],
            "a migrated VM must keep its SSH key associations"
        );
    }

    /// Regression: `DrainNode` had the same delete-then-reinsert, and read the
    /// SSH keys back *after* the delete had already cascaded them, so it lost
    /// both attachments and keys.
    #[tokio::test]
    async fn drain_node_preserves_security_groups_and_ssh_keys() {
        let (db, vm) = ceph_two_node_fixture();
        mark_ceph_cluster_healthy(&db, "lab");
        attach_web_security_group(&db, &vm.id);
        attach_ops_ssh_key(&db, &vm.id);

        let svc = svc_for(&db);
        let resp =
            <ControllerService as controller_proto::controller_server::Controller>::drain_node(
                &svc,
                Request::new(controller_proto::DrainNodeRequest {
                    node_id: "node-a".into(),
                    target_node: "node-b".into(),
                }),
            )
            .await
            .expect("drain should succeed")
            .into_inner();
        assert!(resp.success, "{}", resp.message);
        assert_eq!(db.get_vm(&vm.id).unwrap().unwrap().node_id, "node-b");
        assert_eq!(
            db.list_security_groups_for_vm(&vm.id).unwrap(),
            vec!["web".to_string()],
            "a drained VM must keep its security group attachments"
        );
        assert_eq!(
            db.get_vm_ssh_key_names(&vm.id).unwrap(),
            vec!["ops".to_string()],
            "a drained VM must keep its SSH key associations"
        );
    }

    #[tokio::test]
    async fn migrate_vm_rejects_a_target_that_is_being_drained() {
        let (db, vm) = ceph_two_node_fixture();
        mark_ceph_cluster_healthy(&db, "lab");
        db.update_node_status("node-b", "draining").unwrap();
        let svc = svc_for(&db);
        let err =
            <ControllerService as controller_proto::controller_server::Controller>::migrate_vm(
                &svc,
                Request::new(controller_proto::MigrateVmRequest {
                    vm_id: vm.id.clone(),
                    target_node: "node-b".into(),
                    allow_cold_fallback: true,
                    gpus: vec![],
                    vgpus: vec![],
                    ..Default::default()
                }),
            )
            .await
            .expect_err("a draining node must not be a migration target");
        assert_eq!(err.code(), tonic::Code::FailedPrecondition);
        assert!(
            err.message().contains("evacuated"),
            "unexpected message: {}",
            err.message()
        );
        assert_eq!(db.get_vm(&vm.id).unwrap().unwrap().node_id, "node-a");
    }

    fn receive_state(
        f: impl FnOnce(&mut controller_proto::LiveMigrateReceiveState),
    ) -> controller_proto::LiveMigrateReceiveState {
        let mut s = controller_proto::LiveMigrateReceiveState::default();
        f(&mut s);
        s
    }

    /// The gate that makes the escape hatch safe to run by accident: without
    /// force it reports and clears nothing.
    #[test]
    fn plan_reset_only_clears_with_explicit_force() {
        let stranded = receive_state(|s| {
            s.has_session = true;
            s.port = 18000;
        });
        assert_eq!(plan_reset(Some(&stranded), false), ResetPlan::ReportOnly);
        assert_eq!(plan_reset(Some(&stranded), true), ResetPlan::Clear);
    }

    /// Idempotent: a clean node is a success under either flag, never an
    /// error a retried runbook step has to special-case.
    #[test]
    fn plan_reset_treats_a_clean_node_as_nothing_to_clear() {
        let clean = controller_proto::LiveMigrateReceiveState::default();
        assert_eq!(plan_reset(Some(&clean), false), ResetPlan::NothingToClear);
        assert_eq!(plan_reset(Some(&clean), true), ResetPlan::NothingToClear);
    }

    /// A live receive is still clearable — the operator decides — but only
    /// with force, and the leftovers must be visible either way.
    #[test]
    fn plan_reset_reports_a_live_receive_before_clearing_it() {
        let live = receive_state(|s| {
            s.has_session = true;
            s.port = 18000;
            s.port_listening = true;
            s.vmm_alive = true;
            s.vmm_pid_matches_vm = true;
        });
        assert!(receive_looks_live(&live));
        assert_eq!(plan_reset(Some(&live), false), ResetPlan::ReportOnly);
        assert_eq!(plan_reset(Some(&live), true), ResetPlan::Clear);
    }

    /// Files left behind without an in-memory session are exactly the
    /// node-agent-restarted case, and still need clearing.
    #[test]
    fn plan_reset_counts_leftover_files_as_something_to_clear() {
        let orphan = receive_state(|s| s.pid_file_present = true);
        assert_eq!(plan_reset(Some(&orphan), true), ResetPlan::Clear);
        let marker = receive_state(|s| s.marker_present = true);
        assert_eq!(plan_reset(Some(&marker), true), ResetPlan::Clear);
    }

    /// A pid that is alive but belongs to something else was recycled; it must
    /// not be reported to the operator as an in-flight migration.
    #[test]
    fn receive_looks_live_ignores_a_recycled_pid() {
        let recycled = receive_state(|s| {
            s.has_session = true;
            s.vmm_alive = true;
            s.vmm_pid_matches_vm = false;
        });
        assert!(!receive_looks_live(&recycled));
    }

    #[tokio::test]
    async fn reset_live_migrate_receive_requires_an_explicit_node() {
        let (db, vm) = ceph_two_node_fixture();
        let svc = svc_for(&db);
        let err = <ControllerService as controller_proto::controller_server::Controller>::
            reset_live_migrate_receive(
                &svc,
                Request::new(controller_proto::ResetLiveMigrateReceiveRequest {
                    vm_id: vm.id.clone(),
                    node_id: "  ".into(),
                    force: true,
                }),
            )
            .await
            .expect_err("clearing must name the node it destroys state on");
        assert_eq!(err.code(), tonic::Code::InvalidArgument);
        assert!(
            err.message().contains("node_id is required"),
            "unexpected message: {}",
            err.message()
        );
    }

    /// The dangerous case. A receive session only strands on a node that does
    /// not own the VM, so a request against the owner is a mistake — and the
    /// cleanup there would stop a VM that is running perfectly well.
    #[tokio::test]
    async fn reset_live_migrate_receive_refuses_the_node_that_owns_the_vm() {
        let (db, vm) = ceph_two_node_fixture();
        let svc = svc_for(&db);
        let err = <ControllerService as controller_proto::controller_server::Controller>::
            reset_live_migrate_receive(
                &svc,
                Request::new(controller_proto::ResetLiveMigrateReceiveRequest {
                    vm_id: vm.id.clone(),
                    // node-a is where ceph_two_node_fixture puts the VM.
                    node_id: "node-a".into(),
                    force: true,
                }),
            )
            .await
            .expect_err("the owning node must be refused");
        assert_eq!(err.code(), tonic::Code::FailedPrecondition);
        assert!(
            err.message().contains("currently owns"),
            "unexpected message: {}",
            err.message()
        );
        // Nothing moved: the VM is still where it was.
        assert_eq!(db.get_vm(&vm.id).unwrap().unwrap().node_id, "node-a");
    }

    #[tokio::test]
    async fn reset_live_migrate_receive_reports_an_unknown_vm_and_node() {
        let (db, vm) = ceph_two_node_fixture();
        let svc = svc_for(&db);

        let err = <ControllerService as controller_proto::controller_server::Controller>::
            reset_live_migrate_receive(
                &svc,
                Request::new(controller_proto::ResetLiveMigrateReceiveRequest {
                    vm_id: "no-such-vm".into(),
                    node_id: "node-b".into(),
                    force: true,
                }),
            )
            .await
            .expect_err("unknown VM");
        assert_eq!(err.code(), tonic::Code::NotFound);

        let err = <ControllerService as controller_proto::controller_server::Controller>::
            reset_live_migrate_receive(
                &svc,
                Request::new(controller_proto::ResetLiveMigrateReceiveRequest {
                    vm_id: vm.name.clone(),
                    node_id: "no-such-node".into(),
                    force: true,
                }),
            )
            .await
            .expect_err("unknown node");
        assert_eq!(err.code(), tonic::Code::NotFound);
    }

    /// With no node named, the report covers every CephCluster member, since
    /// those are the only nodes live migrate ever prepares a receive on.
    #[tokio::test]
    async fn receive_status_reports_every_ceph_member_and_marks_the_owner() {
        let (db, vm) = ceph_two_node_fixture();
        mark_ceph_cluster_healthy(&db, "lab");
        let svc = svc_for(&db);
        let resp = <ControllerService as controller_proto::controller_server::Controller>::
            get_live_migrate_receive_status(
                &svc,
                Request::new(controller_proto::GetLiveMigrateReceiveStatusRequest {
                    vm_id: vm.name.clone(),
                    node_id: String::new(),
                }),
            )
            .await
            .expect("status must not fail just because nodes are unreachable")
            .into_inner();

        assert!(resp.success);
        assert_eq!(resp.vm_id, vm.id);
        assert_eq!(resp.current_node, "node-a");
        assert_eq!(resp.runtime_name, sanitize_nix_attr_key(&vm.name));
        let ids: Vec<_> = resp.nodes.iter().map(|n| n.node_id.as_str()).collect();
        assert!(
            ids.contains(&"node-a") && ids.contains(&"node-b"),
            "{ids:?}"
        );

        let owner = resp
            .nodes
            .iter()
            .find(|n| n.node_id == "node-a")
            .expect("owner row");
        assert!(owner.owns_vm, "the owning node must be flagged as such");
        assert!(
            !resp
                .nodes
                .iter()
                .find(|n| n.node_id == "node-b")
                .expect("node-b row")
                .owns_vm
        );

        // No node agent is listening in tests, so every row records why it
        // could not be read rather than failing the whole report.
        for node in &resp.nodes {
            assert!(!node.reachable);
            assert!(!node.error.is_empty(), "an unreachable node must say why");
        }
    }

    #[tokio::test]
    async fn receive_status_accepts_an_explicit_node() {
        let (db, vm) = ceph_two_node_fixture();
        let svc = svc_for(&db);
        let resp = <ControllerService as controller_proto::controller_server::Controller>::
            get_live_migrate_receive_status(
                &svc,
                Request::new(controller_proto::GetLiveMigrateReceiveStatusRequest {
                    vm_id: vm.id.clone(),
                    node_id: "node-b".into(),
                }),
            )
            .await
            .expect("status")
            .into_inner();
        assert_eq!(resp.nodes.len(), 1);
        assert_eq!(resp.nodes[0].node_id, "node-b");
    }

    #[tokio::test]
    async fn migrate_vm_rejects_an_unapproved_target_node() {
        let (db, vm) = ceph_two_node_fixture();
        mark_ceph_cluster_healthy(&db, "lab");
        db.set_node_approval("node-b", "pending").unwrap();
        let svc = svc_for(&db);
        let err =
            <ControllerService as controller_proto::controller_server::Controller>::migrate_vm(
                &svc,
                Request::new(controller_proto::MigrateVmRequest {
                    vm_id: vm.id.clone(),
                    target_node: "node-b".into(),
                    allow_cold_fallback: true,
                    gpus: vec![],
                    vgpus: vec![],
                    ..Default::default()
                }),
            )
            .await
            .expect_err("an unapproved node must not be a migration target");
        assert_eq!(err.code(), tonic::Code::FailedPrecondition);
        assert!(
            err.message().contains("not approved"),
            "unexpected message: {}",
            err.message()
        );
        assert_eq!(db.get_vm(&vm.id).unwrap().unwrap().node_id, "node-a");
    }

    /// An unusable explicit target used to abort the whole RPC mid-loop, after
    /// earlier VMs had already been reassigned and before any config was
    /// pushed. Now it is a per-VM error and the node stays `draining`.
    #[tokio::test]
    async fn drain_node_reports_an_unusable_target_instead_of_aborting() {
        let (db, vm) = ceph_two_node_fixture();
        mark_ceph_cluster_healthy(&db, "lab");
        db.update_node_status("node-b", "draining").unwrap();
        let svc = svc_for(&db);
        let resp =
            <ControllerService as controller_proto::controller_server::Controller>::drain_node(
                &svc,
                Request::new(controller_proto::DrainNodeRequest {
                    node_id: "node-a".into(),
                    target_node: "node-b".into(),
                }),
            )
            .await
            .expect("drain reports per-VM errors rather than failing outright")
            .into_inner();
        assert!(!resp.success, "{}", resp.message);
        assert_eq!(resp.vms_migrated, 0);
        assert!(
            resp.message.contains(&vm.name),
            "message should name the stranded VM: {}",
            resp.message
        );
        assert_eq!(db.get_vm(&vm.id).unwrap().unwrap().node_id, "node-a");
        assert_eq!(db.get_node("node-a").unwrap().unwrap().status, "draining");
    }

    /// The drain is not finished until the source node has actually applied the
    /// configuration that removes the VM units.
    #[tokio::test]
    async fn drain_node_stays_draining_when_the_config_push_fails() {
        let (db, vm) = ceph_two_node_fixture();
        mark_ceph_cluster_healthy(&db, "lab");
        let hook: PushHook = Arc::new(|n: &NodeRow| {
            if n.id == "node-a" {
                Err(Status::deadline_exceeded("rebuild never activated"))
            } else {
                Ok(())
            }
        });
        let svc = ControllerService::new_with_test_push_hook(
            db.clone(),
            NodeClients::new(None),
            test_network(),
            None,
            false,
            hook,
        );
        let resp =
            <ControllerService as controller_proto::controller_server::Controller>::drain_node(
                &svc,
                Request::new(controller_proto::DrainNodeRequest {
                    node_id: "node-a".into(),
                    target_node: "node-b".into(),
                }),
            )
            .await
            .expect("drain surfaces push failures in its response")
            .into_inner();
        assert!(
            !resp.success,
            "a node whose rebuild failed is not drained: {}",
            resp.message
        );
        assert!(
            resp.message.contains("post-drain configuration"),
            "unexpected message: {}",
            resp.message
        );
        assert_eq!(
            db.get_node("node-a").unwrap().unwrap().status,
            "draining",
            "a node that never applied its post-drain config must not be marked drained"
        );
        // The VM itself did move; only the source's apply is outstanding.
        assert_eq!(db.get_vm(&vm.id).unwrap().unwrap().node_id, "node-b");
    }

    #[test]
    fn release_barrier_only_passes_when_both_post_conditions_hold() {
        let resp = |vmm_stopped, rbd_unmapped| node_proto::FinalizeLiveMigrateSourceResponse {
            success: vmm_stopped && rbd_unmapped,
            message: "observed".into(),
            vmm_stopped,
            rbd_unmapped,
        };
        assert!(check_release_barrier("web-1", "node-a", &resp(true, true)).is_ok());
        for (vmm_stopped, rbd_unmapped) in [(false, true), (true, false), (false, false)] {
            let err = check_release_barrier("web-1", "node-a", &resp(vmm_stopped, rbd_unmapped))
                .expect_err("an unreleased source must be a hard failure");
            assert_eq!(err.code(), tonic::Code::FailedPrecondition);
            assert!(
                err.message().contains("web-1") && err.message().contains("node-a"),
                "unexpected message: {}",
                err.message()
            );
        }
    }

    #[test]
    fn nix_apply_progress_maps_every_phase() {
        assert_eq!(
            nix_apply_progress(node_proto::NixApplyPhase::Succeeded as i32),
            NixApplyProgress::Activated
        );
        assert_eq!(
            nix_apply_progress(node_proto::NixApplyPhase::Failed as i32),
            NixApplyProgress::Failed
        );
        assert_eq!(
            nix_apply_progress(node_proto::NixApplyPhase::Running as i32),
            NixApplyProgress::Pending
        );
        assert_eq!(
            nix_apply_progress(node_proto::NixApplyPhase::Unknown as i32),
            NixApplyProgress::NoVerdict
        );
        // Unset, and anything a newer node agent invents, must keep the caller
        // polling rather than silently declaring success.
        assert_eq!(
            nix_apply_progress(node_proto::NixApplyPhase::Unspecified as i32),
            NixApplyProgress::Pending
        );
        assert_eq!(nix_apply_progress(9999), NixApplyProgress::Pending);
    }

    #[test]
    fn live_migrate_cpu_check_requires_flags_the_source_guest_can_see() {
        let source = ["sse4_2", "avx", "vmx"]
            .into_iter()
            .map(str::to_string)
            .collect::<Vec<_>>();
        let dest = ["sse4_2"]
            .into_iter()
            .map(str::to_string)
            .collect::<Vec<_>>();
        let missing = ControllerService::cpu_flags_missing_on_dest(&source, &dest);
        assert!(missing.contains(&"avx".to_string()));
        assert!(!missing.iter().any(|flag| flag == "vmx"));
        assert!(ControllerService::cpu_flags_missing_on_dest(&source, &source).is_empty());
    }

    #[tokio::test]
    async fn delete_ceph_cluster_refuses_while_ceph_vms_still_use_it() {
        let (db, vm) = ceph_two_node_fixture();
        let svc = svc_for(&db);
        let err = <ControllerService as controller_proto::controller_server::Controller>::delete_ceph_cluster(
            &svc,
            Request::new(controller_proto::DeleteCephClusterRequest {
                name: "lab".into(),
                force: false,
            }),
        )
        .await
        .expect_err("deleting a cluster under a live VM must be refused");
        assert_eq!(err.code(), tonic::Code::FailedPrecondition);
        assert!(
            err.message().contains(&vm.name),
            "message should name the blocking VM: {}",
            err.message()
        );
        assert!(db.get_ceph_cluster("lab").unwrap().is_some());
    }

    #[tokio::test]
    async fn delete_ceph_cluster_succeeds_once_ceph_vms_are_gone() {
        let (db, vm) = ceph_two_node_fixture();
        db.delete_vm_by_id_or_name(&vm.id).unwrap();
        let svc = svc_for(&db);
        let resp = <ControllerService as controller_proto::controller_server::Controller>::delete_ceph_cluster(
            &svc,
            Request::new(controller_proto::DeleteCephClusterRequest {
                name: "lab".into(),
                force: true,
            }),
        )
        .await
        .expect("delete should succeed with no Ceph VMs")
        .into_inner();
        assert!(resp.success);
        assert!(db.get_ceph_cluster("lab").unwrap().is_none());
    }

    #[tokio::test]
    async fn delete_ceph_cluster_ignores_non_ceph_vms_on_member_nodes() {
        let (db, vm) = ceph_two_node_fixture();
        db.delete_vm_by_id_or_name(&vm.id).unwrap();
        let mut local = test_vm("node-a");
        local.id = "vm-local-1".into();
        local.name = "local-1".into();
        local.storage_backend = "lvm".into();
        db.insert_vm(&local).unwrap();

        let svc = svc_for(&db);
        let resp = <ControllerService as controller_proto::controller_server::Controller>::delete_ceph_cluster(
            &svc,
            Request::new(controller_proto::DeleteCephClusterRequest {
                name: "lab".into(),
                force: true,
            }),
        )
        .await
        .expect("an LVM VM must not block cluster deletion")
        .into_inner();
        assert!(resp.success);
    }

    #[tokio::test]
    async fn migrate_vm_cold_fallback_reassigns_node() {
        let db = Database::open(":memory:").expect("open db");
        let mut node_a = test_node();
        node_a.id = "node-a".into();
        db.upsert_node(&node_a).unwrap();
        let mut node_b = test_node();
        node_b.id = "node-b".into();
        node_b.address = "127.0.0.2:59991".into();
        db.upsert_node(&node_b).unwrap();

        let mut spec = make_ceph_spec(&["node-a", "node-b"]);
        spec.fsid = "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee".into();
        let spec_json = crate::ceph_cluster_spec::spec_to_json(&spec).unwrap();
        db.upsert_ceph_cluster(&CephClusterRow {
            name: "lab".into(),
            generation: 1,
            spec_json,
            bootstrap_json: String::new(),
            created_at: String::new(),
            updated_at: String::new(),
        })
        .unwrap();
        mark_ceph_cluster_healthy(&db, "lab");

        let mut vm = test_vm("node-a");
        vm.id = "vm-mig-1".into();
        vm.name = "mig-1".into();
        vm.storage_backend = "ceph".into();
        db.insert_vm(&vm).unwrap();
        db.upsert_volume(&test_root_volume(
            "vol-1",
            &vm.id,
            &vm.name,
            8 * 1024 * 1024 * 1024,
        ))
        .unwrap();

        let hook: PushHook = Arc::new(|_: &NodeRow| Ok(()));
        let svc = ControllerService::new_with_test_push_hook(
            db.clone(),
            NodeClients::new(None),
            test_network(),
            None,
            false,
            hook,
        );
        let resp =
            <ControllerService as controller_proto::controller_server::Controller>::migrate_vm(
                &svc,
                Request::new(controller_proto::MigrateVmRequest {
                    vm_id: vm.id.clone(),
                    target_node: "node-b".into(),
                    allow_cold_fallback: true,
                    gpus: vec![],
                    vgpus: vec![],
                    ..Default::default()
                }),
            )
            .await
            .expect("cold fallback should succeed")
            .into_inner();
        assert!(resp.success);
        assert_eq!(resp.mode, "cold");
        assert_eq!(resp.target_node, "node-b");
        let moved = db.get_vm(&vm.id).unwrap().unwrap();
        assert_eq!(moved.node_id, "node-b");
    }

    #[tokio::test]
    async fn delete_vm_by_name_removes_ceph_volume_row() {
        let db = Database::open(":memory:").expect("open db");
        let node = test_node();
        db.upsert_node(&node).unwrap();
        let mut vm = test_vm(&node.id);
        vm.id = "vm-del-1".into();
        vm.name = "del-by-name".into();
        vm.storage_backend = "ceph".into();
        db.insert_vm(&vm).unwrap();
        db.upsert_volume(&test_root_volume("vol-del", &vm.id, &vm.name, 1024))
            .unwrap();
        let hook: PushHook = Arc::new(|_: &NodeRow| Ok(()));
        let svc = ControllerService::new_with_test_push_hook(
            db.clone(),
            NodeClients::new(None),
            test_network(),
            None,
            false,
            hook,
        );
        let resp =
            <ControllerService as controller_proto::controller_server::Controller>::delete_vm(
                &svc,
                Request::new(controller_proto::DeleteVmRequest {
                    vm_id: "del-by-name".into(),
                    target_node: String::new(),
                    delete_data_volumes: false,
                }),
            )
            .await
            .expect("delete by name")
            .into_inner();
        assert!(resp.success);
        assert!(db.get_vm(&vm.id).unwrap().is_none());
        assert!(
            db.get_volume_by_vm(&vm.id).unwrap().is_none(),
            "volume row must be deleted when VM is deleted by name"
        );
    }

    #[test]
    fn rollback_created_vm_clears_volume_row_without_node_rpc() {
        let db = Database::open(":memory:").expect("open db");
        let node = test_node();
        db.upsert_node(&node).unwrap();
        let mut vm = test_vm(&node.id);
        vm.id = "vm-rb".into();
        vm.storage_backend = "ceph".into();
        db.insert_vm(&vm).unwrap();
        db.upsert_volume(&test_root_volume("vol-rb", &vm.id, &vm.name, 1024))
            .unwrap();
        let hook: PushHook = Arc::new(|_: &NodeRow| Ok(()));
        let svc = ControllerService::new_with_test_push_hook(
            db.clone(),
            NodeClients::new(None),
            test_network(),
            None,
            false,
            hook,
        );
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(svc.rollback_created_vm(&node, &vm));
        assert!(db.get_vm(&vm.id).unwrap().is_none());
        assert!(db.get_volume_by_vm(&vm.id).unwrap().is_none());
    }

    #[tokio::test]
    async fn create_network_stores_vxlan_type_and_vni() {
        let db = Database::open(":memory:").expect("open db");
        let node = test_node();
        db.upsert_node(&node).expect("insert node");

        let hook: PushHook = Arc::new(|_n: &NodeRow| Ok(()));
        let svc = ControllerService::new_with_test_push_hook(
            db.clone(),
            NodeClients::new(None),
            test_network(),
            None,
            false,
            hook,
        );

        let resp =
            <ControllerService as controller_proto::controller_server::Controller>::create_network(
                &svc,
                Request::new(controller_proto::CreateNetworkRequest {
                    name: "overlay-1".to_string(),
                    external_ip: "203.0.113.10".to_string(),
                    gateway_ip: "10.250.0.1".to_string(),
                    internal_netmask: "255.255.255.0".to_string(),
                    target_node: node.id.clone(),
                    allowed_tcp_ports: vec![],
                    allowed_udp_ports: vec![],
                    vlan_id: 0,
                    network_type: "vxlan".to_string(),
                    enable_outbound_nat: true,
                    ..Default::default()
                }),
            )
            .await
            .expect("create vxlan network")
            .into_inner();

        assert!(resp.success);

        let net = db
            .get_network_for_node(&node.id, "overlay-1")
            .expect("get network")
            .expect("network exists");
        assert_eq!(net.network_type, "vxlan");
        assert!(net.vni >= 10000 && net.vni <= 15999, "vni={}", net.vni);
        assert!(net.enable_outbound_nat);
        assert_eq!(net.next_ip, 2);
    }

    #[tokio::test]
    async fn create_network_stores_ipv6_and_east_west() {
        let db = Database::open(":memory:").expect("open db");
        let node = test_node();
        db.upsert_node(&node).expect("insert node");
        let hook: PushHook = Arc::new(|_n: &NodeRow| Ok(()));
        let svc = ControllerService::new_with_test_push_hook(
            db.clone(),
            NodeClients::new(None),
            test_network(),
            None,
            false,
            hook,
        );
        let resp =
            <ControllerService as controller_proto::controller_server::Controller>::create_network(
                &svc,
                Request::new(controller_proto::CreateNetworkRequest {
                    name: "overlay-6".into(),
                    external_ip: "203.0.113.10".into(),
                    gateway_ip: "10.250.0.1".into(),
                    internal_netmask: "255.255.255.0".into(),
                    target_node: node.id.clone(),
                    network_type: "vxlan".into(),
                    ipv6_prefix: "fd00:10:240::/64".into(),
                    ipv6_gateway: "fd00:10:240::1".into(),
                    east_west_firewall: true,
                    ..Default::default()
                }),
            )
            .await
            .expect("create")
            .into_inner();
        assert!(resp.success);
        let policy = db
            .get_network_policy(&node.id, "overlay-6")
            .expect("policy");
        assert!(policy.east_west);
        assert_eq!(policy.ipv6_prefix, "fd00:10:240::/64");
        assert_eq!(policy.ipv6_gateway, "fd00:10:240::1");

        let err =
            <ControllerService as controller_proto::controller_server::Controller>::create_network(
                &svc,
                Request::new(controller_proto::CreateNetworkRequest {
                    name: "overlay-6".into(),
                    external_ip: "203.0.113.10".into(),
                    gateway_ip: "10.250.0.1".into(),
                    internal_netmask: "255.255.255.0".into(),
                    target_node: node.id.clone(),
                    network_type: "vxlan".into(),
                    ipv6_prefix: "fd00:10:240::/64".into(),
                    ipv6_gateway: "fd00:10:240::2".into(),
                    east_west_firewall: true,
                    ..Default::default()
                }),
            )
            .await
            .expect_err("bad gateway");
        assert_eq!(err.code(), tonic::Code::InvalidArgument);
    }

    #[tokio::test]
    async fn create_network_rejects_invalid_type() {
        let db = Database::open(":memory:").expect("open db");
        let node = test_node();
        db.upsert_node(&node).expect("insert node");

        let hook: PushHook = Arc::new(|_n: &NodeRow| Ok(()));
        let svc = ControllerService::new_with_test_push_hook(
            db.clone(),
            NodeClients::new(None),
            test_network(),
            None,
            false,
            hook,
        );

        let err =
            <ControllerService as controller_proto::controller_server::Controller>::create_network(
                &svc,
                Request::new(controller_proto::CreateNetworkRequest {
                    name: "bad-net".to_string(),
                    external_ip: "203.0.113.10".to_string(),
                    gateway_ip: "10.250.0.1".to_string(),
                    internal_netmask: "255.255.255.0".to_string(),
                    target_node: node.id.clone(),
                    allowed_tcp_ports: vec![],
                    allowed_udp_ports: vec![],
                    vlan_id: 0,
                    network_type: "wireguard".to_string(),
                    enable_outbound_nat: false,
                    ..Default::default()
                }),
            )
            .await
            .expect_err("invalid type should be rejected");

        assert_eq!(err.code(), tonic::Code::InvalidArgument);
    }

    #[tokio::test]
    async fn create_vm_allocates_ip_for_vxlan_network() {
        let db = Database::open(":memory:").expect("open db");
        let node = test_node();
        db.upsert_node(&node).expect("insert node");

        let hook: PushHook = Arc::new(|_n: &NodeRow| Ok(()));
        let svc = ControllerService::new_with_test_push_hook(
            db.clone(),
            NodeClients::new(None),
            test_network(),
            None,
            false,
            hook,
        );

        <ControllerService as controller_proto::controller_server::Controller>::create_network(
            &svc,
            Request::new(controller_proto::CreateNetworkRequest {
                name: "vx-net".to_string(),
                external_ip: "203.0.113.10".to_string(),
                gateway_ip: "10.250.0.1".to_string(),
                internal_netmask: "255.255.255.0".to_string(),
                target_node: node.id.clone(),
                allowed_tcp_ports: vec![],
                allowed_udp_ports: vec![],
                vlan_id: 0,
                network_type: "vxlan".to_string(),
                enable_outbound_nat: true,
                ..Default::default()
            }),
        )
        .await
        .expect("create vxlan network");

        let create_resp =
            <ControllerService as controller_proto::controller_server::Controller>::create_vm(
                &svc,
                Request::new(controller_proto::CreateVmRequest {
                    spec: Some(controller_proto::VmSpec {
                        id: String::new(),
                        name: "app-1".to_string(),
                        cpu: 1,
                        memory_bytes: 512 * 1024 * 1024,
                        disks: vec![],
                        nics: vec![controller_proto::Nic {
                            network: "vx-net".to_string(),
                            model: String::new(),
                            mac_address: String::new(),
                        }],
                        storage_backend: String::new(),
                        storage_size_bytes: 0,
                        desired_state: controller_proto::VmDesiredState::Unspecified as i32,
                        pci_devices: vec![],
                        gpus: vec![],
                        labels: std::collections::HashMap::new(),
                        vgpus: vec![],
                    }),
                    image_url: "https://example.com/img.raw".to_string(),
                    image_sha256:
                        "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"
                            .to_string(),
                    cloud_init_user_data: String::new(),
                    target_node: node.id.clone(),
                    ssh_key_names: vec![],
                    storage_backend: controller_proto::StorageBackendType::Filesystem as i32,
                    storage_size_bytes: 10 * 1024 * 1024 * 1024,
                    image_path: String::new(),
                    image_format: String::new(),
                    target_dc: String::new(),
                    node_labels: Vec::new(),
                    anti_affinity: String::new(),
                    ..Default::default()
                }),
            )
            .await
            .expect("create vm on vxlan network")
            .into_inner();

        let vm_id = create_resp.vm_id;
        let vm = db.get_vm(&vm_id).expect("get vm").expect("vm exists");
        assert_eq!(vm.vm_ip, "10.250.0.2");
    }

    #[tokio::test]
    async fn create_network_rejects_vxlan_on_disabled_node() {
        let db = Database::open(":memory:").expect("open db");
        let mut node = test_node();
        node.disable_vxlan = true;
        db.upsert_node(&node).expect("insert node");

        let hook: PushHook = Arc::new(|_n: &NodeRow| Ok(()));
        let svc = ControllerService::new_with_test_push_hook(
            db.clone(),
            NodeClients::new(None),
            test_network(),
            None,
            false,
            hook,
        );

        let err =
            <ControllerService as controller_proto::controller_server::Controller>::create_network(
                &svc,
                Request::new(controller_proto::CreateNetworkRequest {
                    name: "overlay-blocked".to_string(),
                    external_ip: "203.0.113.10".to_string(),
                    gateway_ip: "10.250.0.1".to_string(),
                    internal_netmask: "255.255.255.0".to_string(),
                    target_node: node.id.clone(),
                    allowed_tcp_ports: vec![],
                    allowed_udp_ports: vec![],
                    vlan_id: 0,
                    network_type: "vxlan".to_string(),
                    enable_outbound_nat: false,
                    ..Default::default()
                }),
            )
            .await
            .expect_err("vxlan should be rejected on disabled node");

        assert_eq!(err.code(), tonic::Code::FailedPrecondition);
        assert!(err.message().contains("VXLAN is disabled"));
    }

    #[tokio::test]
    async fn create_network_allows_nat_on_vxlan_disabled_node() {
        let db = Database::open(":memory:").expect("open db");
        let mut node = test_node();
        node.disable_vxlan = true;
        db.upsert_node(&node).expect("insert node");

        let hook: PushHook = Arc::new(|_n: &NodeRow| Ok(()));
        let svc = ControllerService::new_with_test_push_hook(
            db.clone(),
            NodeClients::new(None),
            test_network(),
            None,
            false,
            hook,
        );

        let resp =
            <ControllerService as controller_proto::controller_server::Controller>::create_network(
                &svc,
                Request::new(controller_proto::CreateNetworkRequest {
                    name: "nat-allowed".to_string(),
                    external_ip: "203.0.113.10".to_string(),
                    gateway_ip: "10.250.0.1".to_string(),
                    internal_netmask: "255.255.255.0".to_string(),
                    target_node: node.id.clone(),
                    allowed_tcp_ports: vec![],
                    allowed_udp_ports: vec![],
                    vlan_id: 0,
                    network_type: "nat".to_string(),
                    enable_outbound_nat: false,
                    ..Default::default()
                }),
            )
            .await
            .expect("nat should succeed on vxlan-disabled node")
            .into_inner();

        assert!(resp.success);
    }

    #[tokio::test]
    async fn new_node_auto_approved_by_default() {
        let db = Database::open(":memory:").expect("open db");
        let svc = ControllerService::new(
            db.clone(),
            NodeClients::new(None),
            test_network(),
            empty_sub_ca(),
            None,
            false,
            false,
        );

        let resp =
            <ControllerService as controller_proto::controller_server::Controller>::register_node(
                &svc,
                Request::new(controller_proto::RegisterNodeRequest {
                    node_id: "new-node".to_string(),
                    hostname: "new-node".to_string(),
                    address: "10.0.0.99:9091".to_string(),
                    capacity: Some(controller_proto::NodeCapacity {
                        cpu_cores: 4,
                        memory_bytes: 8_000_000_000,
                    }),
                    labels: vec![],
                    storage_backend: 1,
                    disable_vxlan: false,
                    cert_expiry_days: 365,
                    luks_method: String::new(),
                    dc_id: String::new(),
                }),
            )
            .await
            .expect("register should succeed")
            .into_inner();

        assert!(resp.success);
        assert_eq!(resp.approval_status, "approved");

        let node = db.get_node("new-node").expect("get").expect("exists");
        assert_eq!(node.approval_status, "approved");
        assert_eq!(node.status, "ready");
    }

    #[tokio::test]
    async fn new_node_pending_when_manual_approval_required() {
        let db = Database::open(":memory:").expect("open db");
        let svc = ControllerService::new(
            db.clone(),
            NodeClients::new(None),
            test_network(),
            empty_sub_ca(),
            None,
            true,
            false,
        );

        let resp =
            <ControllerService as controller_proto::controller_server::Controller>::register_node(
                &svc,
                Request::new(controller_proto::RegisterNodeRequest {
                    node_id: "new-node".to_string(),
                    hostname: "new-node".to_string(),
                    address: "10.0.0.99:9091".to_string(),
                    capacity: Some(controller_proto::NodeCapacity {
                        cpu_cores: 4,
                        memory_bytes: 8_000_000_000,
                    }),
                    labels: vec![],
                    storage_backend: 1,
                    disable_vxlan: false,
                    cert_expiry_days: 365,
                    luks_method: String::new(),
                    dc_id: String::new(),
                }),
            )
            .await
            .expect("register should succeed")
            .into_inner();

        assert!(resp.success);
        assert!(resp.message.contains("pending"));

        let node = db.get_node("new-node").expect("get").expect("exists");
        assert_eq!(node.approval_status, "pending");
        assert_eq!(node.status, "pending");
    }

    #[tokio::test]
    async fn register_node_appends_replication_outbox_when_configured() {
        let db = Database::open(":memory:").expect("open db");
        let replication = Some(ReplicationConfig {
            controller_id: "ctrl-test".into(),
            dc_id: "DC1".into(),
            peers: vec![],
        });
        let svc = ControllerService::new(
            db.clone(),
            NodeClients::new(None),
            test_network(),
            empty_sub_ca(),
            replication,
            false,
            false,
        );

        <ControllerService as controller_proto::controller_server::Controller>::register_node(
            &svc,
            Request::new(controller_proto::RegisterNodeRequest {
                node_id: "repl-node".to_string(),
                hostname: "repl-node".to_string(),
                address: "10.0.0.55:9091".to_string(),
                capacity: Some(controller_proto::NodeCapacity {
                    cpu_cores: 2,
                    memory_bytes: 4_000_000_000,
                }),
                labels: vec!["role=test".to_string()],
                storage_backend: 1,
                disable_vxlan: false,
                cert_expiry_days: 365,
                luks_method: String::new(),
                dc_id: String::new(),
            }),
        )
        .await
        .expect("register should succeed");

        assert_eq!(db.replication_outbox_len().expect("count"), 1);
    }

    #[tokio::test]
    async fn heartbeat_appends_replication_outbox_when_configured() {
        let db = Database::open(":memory:").expect("open db");
        let mut node = test_node();
        node.approval_status = "approved".to_string();
        node.status = "ready".to_string();
        db.upsert_node(&node).expect("insert");
        let replication = Some(ReplicationConfig {
            controller_id: "ctrl-test".into(),
            dc_id: "DC1".into(),
            peers: vec![],
        });
        let svc = ControllerService::new(
            db.clone(),
            NodeClients::new(None),
            test_network(),
            empty_sub_ca(),
            replication,
            false,
            false,
        );
        <ControllerService as controller_proto::controller_server::Controller>::heartbeat(
            &svc,
            Request::new(controller_proto::HeartbeatRequest {
                node_id: node.id.clone(),
                usage: Some(controller_proto::NodeUsage {
                    cpu_cores_used: 1,
                    memory_bytes_used: 1024,
                }),
                cert_expiry_days: 300,
                luks_method: "tpm2".to_string(),
                gpus: vec![],
                gpus_reported: false,
                ..Default::default()
            }),
        )
        .await
        .expect("heartbeat should succeed");

        assert_eq!(db.replication_outbox_len().expect("count"), 1);
        let rows = db
            .list_replication_outbox_since(0, 10)
            .expect("list outbox rows");
        assert_eq!(rows[0].event_type, EVT_NODE_HEARTBEAT);
    }

    #[tokio::test]
    async fn approved_node_re_registers_as_approved() {
        let db = Database::open(":memory:").expect("open db");
        let mut node = test_node();
        node.approval_status = "approved".to_string();
        db.upsert_node(&node).expect("insert");

        let svc = ControllerService::new(
            db.clone(),
            NodeClients::new(None),
            test_network(),
            empty_sub_ca(),
            None,
            false,
            false,
        );

        let resp =
            <ControllerService as controller_proto::controller_server::Controller>::register_node(
                &svc,
                Request::new(controller_proto::RegisterNodeRequest {
                    node_id: "node-1".to_string(),
                    hostname: "node-1".to_string(),
                    address: "127.0.0.1:59991".to_string(),
                    capacity: Some(controller_proto::NodeCapacity {
                        cpu_cores: 4,
                        memory_bytes: 8_000_000_000,
                    }),
                    labels: vec![],
                    storage_backend: 1,
                    disable_vxlan: false,
                    cert_expiry_days: 300,
                    luks_method: "tpm2".to_string(),
                    dc_id: String::new(),
                }),
            )
            .await
            .expect("re-register should succeed")
            .into_inner();

        assert!(resp.success);
        assert_eq!(resp.message, "registered");

        let n = db.get_node("node-1").expect("get").expect("exists");
        assert_eq!(n.approval_status, "approved");
        assert_eq!(n.status, "ready");
    }

    /// A node-agent restart re-registers the node. That used to overwrite a
    /// drain or cordon with `ready`, so the node silently came back into
    /// scheduling with no uncordon. Only `UncordonNode` may do that.
    #[tokio::test]
    async fn re_register_keeps_drained_and_cordoned_status_until_uncordon() {
        for held in ["drained", "draining", "cordoned"] {
            let db = Database::open(":memory:").expect("open db");
            let mut node = test_node();
            node.approval_status = "approved".to_string();
            node.status = held.to_string();
            db.upsert_node(&node).expect("insert");
            let svc = ControllerService::new(
                db.clone(),
                NodeClients::new(None),
                test_network(),
                empty_sub_ca(),
                Some(ReplicationConfig {
                    controller_id: "ctrl-test".into(),
                    dc_id: "DC1".into(),
                    peers: vec![],
                }),
                false,
                false,
            );
            let register = || controller_proto::RegisterNodeRequest {
                node_id: "node-1".to_string(),
                hostname: "node-1".to_string(),
                address: "127.0.0.1:59991".to_string(),
                capacity: Some(controller_proto::NodeCapacity {
                    cpu_cores: 4,
                    memory_bytes: 8_000_000_000,
                }),
                labels: vec![],
                storage_backend: 1,
                disable_vxlan: false,
                cert_expiry_days: 300,
                luks_method: "tpm2".to_string(),
                dc_id: String::new(),
            };

            <ControllerService as controller_proto::controller_server::Controller>::register_node(
                &svc,
                Request::new(register()),
            )
            .await
            .expect("re-register");
            let n = db.get_node("node-1").expect("get").expect("exists");
            assert_eq!(n.status, held, "re-register must keep {held}");
            assert_eq!(n.approval_status, "approved");

            // The replicated event must carry the preserved status too, or a
            // follower (and the next primary) would reset it on apply.
            let events = db.list_replication_outbox_since(0, 10).expect("outbox");
            let register_evt = events
                .iter()
                .find(|e| e.event_type == EVT_NODE_REGISTER)
                .expect("node.register event logged");
            let payload: serde_json::Value =
                serde_json::from_slice(&register_evt.payload).expect("payload json");
            assert_eq!(payload["body"]["status"], held);

            <ControllerService as controller_proto::controller_server::Controller>::uncordon_node(
                &svc,
                Request::new(controller_proto::UncordonNodeRequest {
                    node_id: "node-1".to_string(),
                    force: false,
                }),
            )
            .await
            .expect("uncordon");
            let n = db.get_node("node-1").expect("get").expect("exists");
            assert_eq!(n.status, "ready", "uncordon clears {held}");

            <ControllerService as controller_proto::controller_server::Controller>::register_node(
                &svc,
                Request::new(register()),
            )
            .await
            .expect("register after uncordon");
            let n = db.get_node("node-1").expect("get").expect("exists");
            assert_eq!(n.status, "ready");
        }
    }

    #[tokio::test]
    async fn approve_node_transitions_to_ready() {
        let db = Database::open(":memory:").expect("open db");
        let mut node = test_node();
        node.approval_status = "pending".to_string();
        node.status = "pending".to_string();
        db.upsert_node(&node).expect("insert");

        let svc = ControllerService::new(
            db.clone(),
            NodeClients::new(None),
            test_network(),
            empty_sub_ca(),
            None,
            false,
            false,
        );

        let resp =
            <ControllerService as controller_proto::controller_server::Controller>::approve_node(
                &svc,
                Request::new(controller_proto::ApproveNodeRequest {
                    node_id: "node-1".to_string(),
                }),
            )
            .await
            .expect("approve should succeed")
            .into_inner();

        assert!(resp.success);

        let n = db.get_node("node-1").expect("get").expect("exists");
        assert_eq!(n.approval_status, "approved");
        assert_eq!(n.status, "ready");
    }

    #[tokio::test]
    async fn reject_node_marks_rejected() {
        let db = Database::open(":memory:").expect("open db");
        let mut node = test_node();
        node.approval_status = "pending".to_string();
        node.status = "pending".to_string();
        db.upsert_node(&node).expect("insert");

        let svc = ControllerService::new(
            db.clone(),
            NodeClients::new(None),
            test_network(),
            empty_sub_ca(),
            None,
            false,
            false,
        );

        let resp =
            <ControllerService as controller_proto::controller_server::Controller>::reject_node(
                &svc,
                Request::new(controller_proto::RejectNodeRequest {
                    node_id: "node-1".to_string(),
                }),
            )
            .await
            .expect("reject should succeed")
            .into_inner();

        assert!(resp.success);

        let n = db.get_node("node-1").expect("get").expect("exists");
        assert_eq!(n.approval_status, "rejected");
        assert_eq!(n.status, "rejected");
    }

    #[tokio::test]
    async fn cluster_health_reports_a_ready_node() {
        let db = Database::open(":memory:").expect("open db");
        db.upsert_node(&test_node()).expect("insert");
        let svc = svc_for(&db);
        let resp = <ControllerService as controller_proto::controller_server::Controller>::get_cluster_health(
            &svc,
            Request::new(controller_proto::GetClusterHealthRequest {}),
        )
        .await
        .expect("health")
        .into_inner();
        assert_eq!(resp.status, "healthy");
        assert_eq!(resp.ready_nodes, 1);
    }

    #[tokio::test]
    async fn backup_and_restore_round_trip_through_the_rpc() {
        let db = Database::open(":memory:").expect("open db");
        db.upsert_node(&test_node()).expect("insert");
        let svc = svc_for(&db);
        let snap =
            <ControllerService as controller_proto::controller_server::Controller>::backup_cluster(
                &svc,
                Request::new(controller_proto::BackupClusterRequest {}),
            )
            .await
            .expect("backup")
            .into_inner();
        assert!(snap.schema_version >= 1);

        let refused = <ControllerService as controller_proto::controller_server::Controller>::restore_cluster(
            &svc,
            Request::new(controller_proto::RestoreClusterRequest {
                sqlite: snap.sqlite.clone(),
                confirm: false,
            }),
        )
        .await
        .expect_err("confirm required");
        assert_eq!(refused.code(), tonic::Code::FailedPrecondition);

        db.delete_node("node-1").expect("delete");
        assert!(db.get_node("node-1").expect("get").is_none());
        let restored = <ControllerService as controller_proto::controller_server::Controller>::restore_cluster(
            &svc,
            Request::new(controller_proto::RestoreClusterRequest {
                sqlite: snap.sqlite,
                confirm: true,
            }),
        )
        .await
        .expect("restore")
        .into_inner();
        assert!(restored.success);
        assert!(db.get_node("node-1").expect("get").is_some());
    }

    #[tokio::test]
    async fn failover_fences_then_moves_ceph_vms_and_leaves_local_disks() {
        let (db, vm) = ceph_two_node_fixture();
        mark_ceph_cluster_healthy(&db, "lab");
        db.update_node_status("node-a", "not-ready").expect("mark");
        let mut local = test_vm("node-a");
        local.id = "vm-local".into();
        local.name = "local-disk".into();
        local.storage_backend = "filesystem".into();
        db.insert_vm(&local).expect("local vm");
        let fenced = Arc::new(Mutex::new(Vec::<(String, Vec<String>)>::new()));
        let record = Arc::clone(&fenced);
        let hook: FenceHook = Arc::new(move |handle: &str, hosts: &[String]| {
            record
                .lock()
                .unwrap()
                .push((handle.to_string(), hosts.to_vec()));
            Ok(vec![
                "127.0.0.1:0/4242".to_string(),
                "range:10.10.0.11/32".to_string(),
            ])
        });
        let svc = svc_for(&db).with_test_fence_hook(hook);

        let outcome = svc
            .failover_unreachable_node("node-a")
            .await
            .expect("failover");
        assert_eq!(outcome.migrated, 1);
        assert!(outcome.errors.iter().any(|e| e.contains("local-disk")));
        assert_eq!(
            db.get_vm(&vm.id).expect("get").expect("ceph").node_id,
            "node-b"
        );
        assert_eq!(
            db.get_vm("vm-local").expect("get").expect("local").node_id,
            "node-a"
        );
        assert!(db.node_needs_config_push("node-a").expect("flag"));
        assert_eq!(
            db.get_node("node-a").expect("get").expect("source").status,
            "not-ready"
        );

        // The root image was fenced, naming both the gRPC host and the Ceph
        // public address of the lost node, before the VM changed hands.
        let fenced = fenced.lock().unwrap();
        assert_eq!(fenced.len(), 1, "{fenced:?}");
        assert_eq!(fenced[0].0, format!("kcore-vms/kcore-{}", vm.id));
        assert_eq!(
            fenced[0].1,
            vec![
                "10.10.0.11".to_string(),
                "10.20.0.11".to_string(),
                "127.0.0.1".to_string()
            ]
        );
        let hold = db.get_node_fence_hold("node-a").expect("hold");
        assert!(hold.pending);
        assert_eq!(hold.vms, vec!["ceph-1".to_string()]);
    }

    #[tokio::test]
    async fn failover_does_not_move_a_host_storage_still_sees() {
        let (db, vm) = ceph_two_node_fixture();
        mark_ceph_cluster_healthy(&db, "lab");
        db.update_node_status("node-a", "not-ready").expect("mark");
        let hook: FenceHook = Arc::new(|_handle: &str, _hosts: &[String]| {
            panic!("a live storage heartbeat must not fence");
        });
        let svc = svc_for(&db)
            .with_test_fence_hook(hook)
            .with_test_storage_verdict(crate::ha::StorageVerdict::Alive);

        let outcome = svc
            .failover_unreachable_node("node-a")
            .await
            .expect("failover pass");
        assert_eq!(outcome.migrated, 0);
        assert!(outcome.errors.is_empty(), "{:?}", outcome.errors);
        assert_eq!(
            db.get_vm(&vm.id).expect("get").expect("ceph").node_id,
            "node-a"
        );
        let events = db.list_ha_events("", "", 20).expect("events");
        assert!(
            events.iter().any(|event| event.phase == "agent-down"),
            "{events:?}"
        );
    }

    #[tokio::test]
    async fn failover_leaves_the_vm_in_place_when_fencing_fails() {
        let (db, vm) = ceph_two_node_fixture();
        mark_ceph_cluster_healthy(&db, "lab");
        db.update_node_status("node-a", "not-ready").expect("mark");
        let hook: FenceHook = Arc::new(|handle: &str, _hosts: &[String]| {
            Err(Status::failed_precondition(format!(
                "{handle} is open from 10.10.0.12:0/7 (host 10.10.0.12), which is not the node being fenced"
            )))
        });
        let svc = svc_for(&db).with_test_fence_hook(hook);

        let outcome = svc
            .failover_unreachable_node("node-a")
            .await
            .expect("failover pass");
        assert_eq!(outcome.migrated, 0);
        assert!(
            outcome
                .errors
                .iter()
                .any(|e| e.contains("could not be fenced") && e.contains("10.10.0.12:0/7")),
            "{:?}",
            outcome.errors
        );
        assert_eq!(
            db.get_vm(&vm.id).expect("get").expect("ceph").node_id,
            "node-a"
        );
        assert!(!db.node_needs_config_push("node-a").expect("flag"));
        let hold = db.get_node_fence_hold("node-a").expect("hold");
        assert!(!hold.pending, "{hold:?}");
    }

    #[tokio::test]
    async fn failover_leaves_the_vm_in_place_when_the_fence_has_no_range() {
        let (db, vm) = ceph_two_node_fixture();
        mark_ceph_cluster_healthy(&db, "lab");
        db.update_node_status("node-a", "not-ready").expect("mark");
        let hook: FenceHook = Arc::new(|_handle: &str, _hosts: &[String]| Ok(Vec::new()));
        let svc = svc_for(&db).with_test_fence_hook(hook);

        let outcome = svc
            .failover_unreachable_node("node-a")
            .await
            .expect("failover pass");
        assert_eq!(outcome.migrated, 0);
        assert!(
            outcome
                .errors
                .iter()
                .any(|e| e.contains("could not be fenced") && e.contains("IP range")),
            "{:?}",
            outcome.errors
        );
        assert_eq!(
            db.get_vm(&vm.id).expect("get").expect("ceph").node_id,
            "node-a"
        );
    }

    #[tokio::test]
    async fn failover_without_a_reachable_ceph_member_does_not_move_the_vm() {
        // No fence hook: the controller really tries the members' gRPC
        // addresses, none of which is listening.
        let (db, vm) = ceph_two_node_fixture();
        mark_ceph_cluster_healthy(&db, "lab");
        db.update_node_status("node-a", "not-ready").expect("mark");
        let svc = svc_for(&db);

        let outcome = svc
            .failover_unreachable_node("node-a")
            .await
            .expect("failover pass");
        assert_eq!(outcome.migrated, 0);
        assert!(
            outcome
                .errors
                .iter()
                .any(|e| e.contains("could not be fenced")),
            "{:?}",
            outcome.errors
        );
        assert_eq!(
            db.get_vm(&vm.id).expect("get").expect("ceph").node_id,
            "node-a"
        );
    }

    #[tokio::test]
    async fn cordon_blocks_scheduling_and_heartbeat_keeps_it() {
        let db = Database::open(":memory:").expect("open db");
        let node = test_node();
        db.upsert_node(&node).expect("insert");
        db.insert_vm(&test_vm(&node.id)).expect("insert vm");
        let svc = svc_for(&db);

        let resp =
            <ControllerService as controller_proto::controller_server::Controller>::cordon_node(
                &svc,
                Request::new(controller_proto::CordonNodeRequest {
                    node_id: node.id.clone(),
                }),
            )
            .await
            .expect("cordon")
            .into_inner();
        assert!(resp.success);

        let n = db.get_node(&node.id).expect("get").expect("exists");
        assert_eq!(n.status, "cordoned");
        assert!(scheduler::select_node(std::slice::from_ref(&n)).is_none());
        let vm = db.get_vm("vm-1").expect("get vm").expect("vm stays");
        assert_eq!(vm.node_id, node.id);

        db.update_heartbeat(&node.id, 1, 2, 30, "tpm2")
            .expect("heartbeat");
        let n = db.get_node(&node.id).expect("get").expect("exists");
        assert_eq!(n.status, "cordoned");

        let again =
            <ControllerService as controller_proto::controller_server::Controller>::cordon_node(
                &svc,
                Request::new(controller_proto::CordonNodeRequest {
                    node_id: node.id.clone(),
                }),
            )
            .await
            .expect("cordon again")
            .into_inner();
        assert!(again.message.contains("already unschedulable"));

        let resp =
            <ControllerService as controller_proto::controller_server::Controller>::uncordon_node(
                &svc,
                Request::new(controller_proto::UncordonNodeRequest {
                    node_id: node.id.clone(),
                    force: false,
                }),
            )
            .await
            .expect("uncordon")
            .into_inner();
        assert!(resp.success);
        let n = db.get_node(&node.id).expect("get").expect("exists");
        assert_eq!(n.status, "ready");
        assert!(scheduler::select_node(&[n]).is_some());
    }

    #[tokio::test]
    async fn delete_node_revokes_certificates_and_refuses_while_busy() {
        let db = Database::open(":memory:").expect("open db");
        let node = test_node();
        db.upsert_node(&node).expect("insert");
        db.record_issued_certificate(&IssuedCertRow {
            serial_hex: "0A0B".to_string(),
            subject_cn: "kcore-node-node-1".to_string(),
            identity_kind: "node".to_string(),
            node_id: node.id.clone(),
            issuer_cn: "kcore-sub-ca".to_string(),
            fingerprint_sha256: "aa".to_string(),
            not_before: "2026-01-01T00:00:00Z".to_string(),
            not_after: "2027-01-01T00:00:00Z".to_string(),
            issued_at: "2026-01-01T00:00:00Z".to_string(),
            status: CERT_STATUS_ACTIVE.to_string(),
            revocation_reason: -1,
            revoked_at: String::new(),
        })
        .expect("record cert");
        db.upsert_node_labels(&node.id, &["dc=DC1".to_string()])
            .expect("labels");
        db.insert_vm(&test_vm(&node.id)).expect("insert vm");
        let svc = svc_for(&db);

        let busy =
            <ControllerService as controller_proto::controller_server::Controller>::delete_node(
                &svc,
                Request::new(controller_proto::DeleteNodeRequest {
                    node_id: node.id.clone(),
                }),
            )
            .await
            .expect_err("busy node");
        assert_eq!(busy.code(), tonic::Code::FailedPrecondition);
        assert!(db.get_node(&node.id).expect("get").is_some());
        let cert = db
            .get_issued_certificate("0A0B")
            .expect("cert")
            .expect("still active");
        assert_eq!(cert.status, CERT_STATUS_ACTIVE);

        db.delete_vm_by_id_or_name("vm-1").expect("delete vm");
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
        let networked =
            <ControllerService as controller_proto::controller_server::Controller>::delete_node(
                &svc,
                Request::new(controller_proto::DeleteNodeRequest {
                    node_id: node.id.clone(),
                }),
            )
            .await
            .expect_err("network remains");
        assert_eq!(networked.code(), tonic::Code::FailedPrecondition);
        db.delete_network(&node.id, "frontend")
            .expect("delete network");

        db.upsert_workload(&WorkloadRow {
            id: "wl-1".to_string(),
            name: "workload-1".to_string(),
            kind: "container".to_string(),
            node_id: node.id.clone(),
            runtime_state: "running".to_string(),
            desired_state: "running".to_string(),
            vm_id: String::new(),
            container_image: "nginx:alpine".to_string(),
            network: "default".to_string(),
            storage_backend: "filesystem".to_string(),
            storage_size_bytes: 1024,
            created_at: String::new(),
        })
        .expect("insert workload");
        let loaded =
            <ControllerService as controller_proto::controller_server::Controller>::delete_node(
                &svc,
                Request::new(controller_proto::DeleteNodeRequest {
                    node_id: node.id.clone(),
                }),
            )
            .await
            .expect_err("workload remains");
        assert_eq!(loaded.code(), tonic::Code::FailedPrecondition);
        db.delete_workload_by_id_or_name("wl-1")
            .expect("delete workload");

        let resp =
            <ControllerService as controller_proto::controller_server::Controller>::delete_node(
                &svc,
                Request::new(controller_proto::DeleteNodeRequest {
                    node_id: node.id.clone(),
                }),
            )
            .await
            .expect("delete")
            .into_inner();
        assert!(resp.success);
        assert_eq!(resp.certificates_revoked, 1);
        assert!(db.get_node(&node.id).expect("get").is_none());
        let cert = db
            .get_issued_certificate("0A0B")
            .expect("cert")
            .expect("revoked row remains");
        assert_eq!(cert.status, CERT_STATUS_REVOKED);
        assert_eq!(cert.revocation_reason, 5);
    }

    #[test]
    fn scheduler_skips_pending_nodes() {
        let mut n = NodeRow {
            id: "pending-node".into(),
            hostname: "pending-node".into(),
            address: "10.0.0.1:9091".into(),
            cpu_cores: 8,
            memory_bytes: 16_000_000_000,
            status: "ready".into(),
            last_heartbeat: String::new(),
            gateway_interface: String::new(),
            cpu_used: 0,
            memory_used: 0,
            storage_backend: "filesystem".into(),
            disable_vxlan: false,
            approval_status: "pending".into(),
            cert_expiry_days: -1,
            luks_method: String::new(),
            dc_id: "DC1".to_string(),
        };
        assert!(
            scheduler::select_node(&[n.clone()]).is_none(),
            "pending node should not be selected"
        );

        n.approval_status = "approved".into();
        assert!(
            scheduler::select_node(&[n]).is_some(),
            "approved node should be selected"
        );
    }

    fn test_sub_ca_state() -> Arc<Mutex<SubCaState>> {
        use rcgen::{BasicConstraints, CertificateParams, DnType, IsCa, Issuer, KeyPair};
        use time::{Duration, OffsetDateTime};

        let mut ca_params = CertificateParams::default();
        ca_params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
        ca_params
            .distinguished_name
            .push(DnType::CommonName, "test-ca");
        ca_params.not_before = OffsetDateTime::now_utc();
        ca_params.not_after = OffsetDateTime::now_utc() + Duration::days(3650);
        let ca_key = KeyPair::generate().unwrap();
        let ca_cert = ca_params.self_signed(&ca_key).unwrap();

        let mut sub_params = CertificateParams::default();
        sub_params.is_ca = IsCa::Ca(BasicConstraints::Constrained(0));
        sub_params
            .distinguished_name
            .push(DnType::CommonName, "test-sub-ca");
        sub_params.not_before = OffsetDateTime::now_utc();
        sub_params.not_after = OffsetDateTime::now_utc() + Duration::days(1825);
        let issuer = Issuer::from_ca_cert_pem(&ca_cert.pem(), ca_key).unwrap();
        let sub_key = KeyPair::generate().unwrap();
        let sub_cert = sub_params.signed_by(&sub_key, &issuer).unwrap();

        Arc::new(Mutex::new(SubCaState {
            cert_pem: sub_cert.pem(),
            key_pem: sub_key.serialize_pem(),
            cert_file: None,
            key_file: None,
        }))
    }

    #[tokio::test]
    async fn renew_node_cert_returns_chain() {
        let db = Database::open(":memory:").expect("open db");
        let node = test_node();
        db.upsert_node(&node).expect("insert");

        let svc = ControllerService::new(
            db.clone(),
            NodeClients::new(None),
            test_network(),
            test_sub_ca_state(),
            None,
            false,
            false,
        );

        let resp =
            <ControllerService as controller_proto::controller_server::Controller>::renew_node_cert(
                &svc,
                Request::new(controller_proto::RenewNodeCertRequest {
                    node_id: "node-1".to_string(),
                }),
            )
            .await
            .expect("renew should succeed")
            .into_inner();

        assert!(resp.success);
        assert!(resp.cert_pem.contains("BEGIN CERTIFICATE"));
        assert!(resp.key_pem.contains("BEGIN PRIVATE KEY"));
        let cert_count = resp.cert_pem.matches("BEGIN CERTIFICATE").count();
        assert_eq!(cert_count, 2, "should contain leaf + sub-CA in chain");
    }

    #[tokio::test]
    async fn renew_node_cert_rejects_unapproved_node() {
        let db = Database::open(":memory:").expect("open db");
        let mut node = test_node();
        node.approval_status = "pending".to_string();
        db.upsert_node(&node).expect("insert");

        let svc = ControllerService::new(
            db.clone(),
            NodeClients::new(None),
            test_network(),
            test_sub_ca_state(),
            None,
            false,
            false,
        );

        let err =
            <ControllerService as controller_proto::controller_server::Controller>::renew_node_cert(
                &svc,
                Request::new(controller_proto::RenewNodeCertRequest {
                    node_id: "node-1".to_string(),
                }),
            )
            .await
            .expect_err("should reject unapproved node");

        assert_eq!(err.code(), tonic::Code::PermissionDenied);
    }

    #[tokio::test]
    async fn renew_node_cert_fails_without_sub_ca() {
        let db = Database::open(":memory:").expect("open db");
        let node = test_node();
        db.upsert_node(&node).expect("insert");

        let svc = ControllerService::new(
            db.clone(),
            NodeClients::new(None),
            test_network(),
            empty_sub_ca(),
            None,
            false,
            false,
        );

        let err =
            <ControllerService as controller_proto::controller_server::Controller>::renew_node_cert(
                &svc,
                Request::new(controller_proto::RenewNodeCertRequest {
                    node_id: "node-1".to_string(),
                }),
            )
            .await
            .expect_err("should fail without sub-CA");

        assert_eq!(err.code(), tonic::Code::Unavailable);
    }

    #[tokio::test]
    async fn rotate_sub_ca_updates_state() {
        let db = Database::open(":memory:").expect("open db");
        let sub_ca = test_sub_ca_state();

        let svc = ControllerService::new(
            db.clone(),
            NodeClients::new(None),
            test_network(),
            sub_ca.clone(),
            None,
            false,
            false,
        );

        let new_state = test_sub_ca_state();
        let new_lock = new_state.lock().unwrap();
        let new_cert = new_lock.cert_pem.clone();
        let new_key = new_lock.key_pem.clone();
        drop(new_lock);

        let resp =
            <ControllerService as controller_proto::controller_server::Controller>::rotate_sub_ca(
                &svc,
                Request::new(controller_proto::RotateSubCaRequest {
                    sub_ca_cert_pem: new_cert.clone(),
                    sub_ca_key_pem: new_key.clone(),
                }),
            )
            .await
            .expect("rotate should succeed")
            .into_inner();

        assert!(resp.success);

        let current = sub_ca.lock().unwrap();
        assert_eq!(current.cert_pem, new_cert);
        assert_eq!(current.key_pem, new_key);
    }

    #[tokio::test]
    async fn rotate_sub_ca_rejects_empty_cert() {
        let db = Database::open(":memory:").expect("open db");
        let svc = ControllerService::new(
            db.clone(),
            NodeClients::new(None),
            test_network(),
            empty_sub_ca(),
            None,
            false,
            false,
        );

        let err =
            <ControllerService as controller_proto::controller_server::Controller>::rotate_sub_ca(
                &svc,
                Request::new(controller_proto::RotateSubCaRequest {
                    sub_ca_cert_pem: String::new(),
                    sub_ca_key_pem: String::new(),
                }),
            )
            .await
            .expect_err("should reject empty cert");

        assert_eq!(err.code(), tonic::Code::InvalidArgument);
    }

    /// Regression: a failing `push_config_to_node` during a desired-state
    /// flip MUST roll the DB back to the original `auto_start`. Without
    /// this, the next idempotent `CreateVm`/`SetDesiredState` would diff
    /// against the already-flipped row, return UNCHANGED, and silently
    /// swallow the unreconciled state.
    #[tokio::test]
    async fn set_vm_desired_state_rolls_back_db_when_push_fails() {
        let db = Database::open(":memory:").expect("open db");
        let node = test_node();
        db.upsert_node(&node).expect("insert node");
        let mut vm = test_vm(&node.id);
        vm.auto_start = true;
        db.insert_vm(&vm).expect("insert vm");

        let push_count = Arc::new(AtomicUsize::new(0));
        let count_clone = Arc::clone(&push_count);
        let hook: PushHook = Arc::new(move |_n: &NodeRow| {
            count_clone.fetch_add(1, Ordering::SeqCst);
            Err(Status::internal("simulated push failure"))
        });

        let svc = ControllerService::new_with_test_push_hook(
            db.clone(),
            NodeClients::new(None),
            test_network(),
            None,
            false,
            hook,
        );

        let req = controller_proto::SetVmDesiredStateRequest {
            vm_id: "vm-1".to_string(),
            desired_state: controller_proto::VmDesiredState::Stopped as i32,
            target_node: node.id.clone(),
        };

        let err =
            <ControllerService as controller_proto::controller_server::Controller>::set_vm_desired_state(
                &svc,
                Request::new(req),
            )
            .await
            .expect_err("push failure must surface to caller");
        assert_eq!(err.code(), tonic::Code::Internal);
        assert_eq!(push_count.load(Ordering::SeqCst), 1);

        let after = db.get_vm("vm-1").expect("get vm").expect("vm exists");
        assert!(
            after.auto_start,
            "auto_start must roll back to original (true) when node push fails"
        );
    }

    /// Regression: an idempotent `CreateVm` whose `target_node` was the
    /// node *address* (e.g. `127.0.0.1:59991`) used to trip the immutable
    /// "target_node" diff because the stored row holds the canonical
    /// node id. Re-applying the same manifest must return UNCHANGED.
    #[tokio::test]
    async fn create_vm_upsert_resolves_target_node_address_to_canonical_id() {
        let db = Database::open(":memory:").expect("open db");
        let node = test_node();
        db.upsert_node(&node).expect("insert node");

        let hook: PushHook = Arc::new(|_n: &NodeRow| Ok(()));
        let svc = ControllerService::new_with_test_push_hook(
            db.clone(),
            NodeClients::new(None),
            test_network(),
            None,
            false,
            hook,
        );

        let make_req = |target_node: String| controller_proto::CreateVmRequest {
            target_node,
            spec: Some(controller_proto::VmSpec {
                id: String::new(),
                name: "vm-addr".to_string(),
                cpu: 1,
                memory_bytes: 512 * 1024 * 1024,
                disks: vec![],
                nics: vec![],
                storage_backend: String::new(),
                storage_size_bytes: 0,
                desired_state: controller_proto::VmDesiredState::Unspecified as i32,
                pci_devices: vec![],
                gpus: vec![],
                labels: std::collections::HashMap::new(),
                vgpus: vec![],
            }),
            image_url: "https://example.com/debian.raw".to_string(),
            image_sha256: "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
                .to_string(),
            cloud_init_user_data: String::new(),
            image_path: String::new(),
            image_format: String::new(),
            ssh_key_names: vec![],
            storage_backend: controller_proto::StorageBackendType::Filesystem as i32,
            storage_size_bytes: 8 * 1024 * 1024 * 1024,
            target_dc: String::new(),
            node_labels: Vec::new(),
            anti_affinity: String::new(),
            ..Default::default()
        };

        let first =
            <ControllerService as controller_proto::controller_server::Controller>::create_vm(
                &svc,
                Request::new(make_req(node.id.clone())),
            )
            .await
            .expect("first create succeeds")
            .into_inner();
        assert_eq!(first.action, controller_proto::ApplyAction::Created as i32);

        // Re-apply the SAME spec, but address the node by its host:port
        // instead of its node id. Before the fix this returned
        // InvalidArgument for an "immutable target_node" change.
        let second =
            <ControllerService as controller_proto::controller_server::Controller>::create_vm(
                &svc,
                Request::new(make_req(node.address.clone())),
            )
            .await
            .expect("re-apply by address must be UNCHANGED, not rejected")
            .into_inner();
        assert_eq!(
            second.action,
            controller_proto::ApplyAction::Unchanged as i32
        );
        assert!(second.changed_fields.is_empty());
    }

    fn make_cluster_update_spec() -> controller_proto::ClusterUpdateSpec {
        controller_proto::ClusterUpdateSpec {
            name: "release-0-3-0".into(),
            target: Some(controller_proto::ClusterUpdateTarget {
                version: "0.3.0".into(),
                flake_ref: "github:kcorehypervisor/kcore/v0.3.0".into(),
                flake_rev: "0123456789abcdef0123456789abcdef01234567".into(),
                nixpkgs_rev: String::new(),
                system_profile: String::new(),
            }),
            selector: Some(controller_proto::ClusterUpdateSelector {
                all_nodes: true,
                ..Default::default()
            }),
            strategy: Some(controller_proto::ClusterUpdateStrategy {
                r#type: controller_proto::ClusterUpdateStrategyType::ClusterUpdateStrategyOneAtATime
                    as i32,
                max_unavailable: 1,
                batch_size: 0,
            }),
            drain_vms: false,
            drain_timeout_seconds: 0,
            activation_mode:
                controller_proto::ClusterUpdateActivationMode::ClusterUpdateActivationSwitch as i32,
            reboot_policy: "if-required".into(),
            approval_policy:
                controller_proto::ClusterUpdateApprovalPolicy::ClusterUpdateApprovalManual as i32,
            automatic_rollback: false,
            ..Default::default()
        }
    }

    fn cluster_svc() -> (Database, ControllerService) {
        let db = Database::open(":memory:").expect("open db");
        db.upsert_node(&test_node()).expect("insert node");
        let mut other = test_node();
        other.id = "node-2".to_string();
        other.hostname = "node-2".to_string();
        db.upsert_node(&other).expect("insert node-2");
        let hook: PushHook = Arc::new(|_n: &NodeRow| Ok(()));
        let svc = ControllerService::new_with_test_push_hook(
            db.clone(),
            NodeClients::new(None),
            test_network(),
            None,
            false,
            hook,
        );
        (db, svc)
    }

    #[tokio::test]
    async fn create_cluster_update_creates_then_unchanged_then_updated() {
        let (db, svc) = cluster_svc();
        let spec = make_cluster_update_spec();

        let created = <ControllerService as controller_proto::controller_server::Controller>::create_cluster_update(
            &svc,
            Request::new(controller_proto::CreateClusterUpdateRequest {
                spec: Some(spec.clone()),
            }),
        )
        .await
        .expect("first create")
        .into_inner();
        assert_eq!(
            created.action,
            controller_proto::ApplyAction::Created as i32
        );
        let cu = created.cluster_update.expect("cluster_update");
        assert_eq!(cu.generation, 1);
        assert_eq!(
            cu.phase,
            controller_proto::ClusterUpdatePhase::Pending as i32,
            "manual approval should leave phase=pending"
        );
        assert_eq!(
            cu.approval_status,
            controller_proto::ClusterUpdateApprovalStatus::ClusterUpdateApprovalAwaiting as i32
        );
        let stored_nodes = db.list_cluster_update_nodes("release-0-3-0").unwrap();
        assert_eq!(
            stored_nodes.len(),
            2,
            "all_nodes must enroll every registered node"
        );

        let same = <ControllerService as controller_proto::controller_server::Controller>::create_cluster_update(
            &svc,
            Request::new(controller_proto::CreateClusterUpdateRequest { spec: Some(spec.clone()) }),
        )
        .await
        .expect("idempotent create")
        .into_inner();
        assert_eq!(same.action, controller_proto::ApplyAction::Unchanged as i32);
        assert!(same.changed_fields.is_empty());

        let mut spec2 = spec.clone();
        spec2.target.as_mut().unwrap().flake_rev =
            "9999999999999999999999999999999999999999".into();
        let updated = <ControllerService as controller_proto::controller_server::Controller>::create_cluster_update(
            &svc,
            Request::new(controller_proto::CreateClusterUpdateRequest { spec: Some(spec2) }),
        )
        .await
        .expect("updated create")
        .into_inner();
        assert_eq!(
            updated.action,
            controller_proto::ApplyAction::Updated as i32
        );
        let cu = updated.cluster_update.expect("cluster_update");
        assert_eq!(cu.generation, 2, "spec change must bump generation");
    }

    #[tokio::test]
    async fn create_cluster_update_rejects_selector_with_no_match() {
        let db = Database::open(":memory:").expect("open db");
        db.upsert_node(&test_node()).expect("insert node");
        let hook: PushHook = Arc::new(|_n: &NodeRow| Ok(()));
        let svc = ControllerService::new_with_test_push_hook(
            db.clone(),
            NodeClients::new(None),
            test_network(),
            None,
            false,
            hook,
        );
        let mut spec = make_cluster_update_spec();
        spec.selector = Some(controller_proto::ClusterUpdateSelector {
            datacenters: vec!["NOPE".into()],
            ..Default::default()
        });
        let err = <ControllerService as controller_proto::controller_server::Controller>::create_cluster_update(
            &svc,
            Request::new(controller_proto::CreateClusterUpdateRequest { spec: Some(spec) }),
        )
        .await
        .expect_err("empty selector match must fail");
        assert_eq!(err.code(), tonic::Code::InvalidArgument);
    }

    #[tokio::test]
    async fn create_cluster_update_with_auto_approval_starts_ready_and_approved() {
        let (_db, svc) = cluster_svc();
        let mut spec = make_cluster_update_spec();
        spec.approval_policy =
            controller_proto::ClusterUpdateApprovalPolicy::ClusterUpdateApprovalAuto as i32;
        let created = <ControllerService as controller_proto::controller_server::Controller>::create_cluster_update(
            &svc,
            Request::new(controller_proto::CreateClusterUpdateRequest { spec: Some(spec) }),
        )
        .await
        .expect("auto approval create")
        .into_inner();
        let cu = created.cluster_update.expect("cluster_update");
        assert_eq!(cu.phase, controller_proto::ClusterUpdatePhase::Ready as i32);
        assert_eq!(
            cu.approval_status,
            controller_proto::ClusterUpdateApprovalStatus::ClusterUpdateApprovalApproved as i32
        );
    }

    #[tokio::test]
    async fn approve_cluster_update_moves_pending_to_ready() {
        let (db, svc) = cluster_svc();
        let spec = make_cluster_update_spec();
        let _ = <ControllerService as controller_proto::controller_server::Controller>::create_cluster_update(
            &svc,
            Request::new(controller_proto::CreateClusterUpdateRequest { spec: Some(spec) }),
        )
        .await
        .expect("create");

        let resp = <ControllerService as controller_proto::controller_server::Controller>::approve_cluster_update(
            &svc,
            Request::new(controller_proto::ApproveClusterUpdateRequest { name: "release-0-3-0".into() }),
        )
        .await
        .expect("approve")
        .into_inner();
        let cu = resp.cluster_update.expect("cluster_update");
        assert_eq!(cu.phase, controller_proto::ClusterUpdatePhase::Ready as i32);
        assert_eq!(
            cu.approval_status,
            controller_proto::ClusterUpdateApprovalStatus::ClusterUpdateApprovalApproved as i32
        );
        // Cluster row must be in `ready` so the reconciler picks it up.
        let row = db.get_cluster_update("release-0-3-0").unwrap().unwrap();
        assert_eq!(row.phase, "ready");
    }

    #[tokio::test]
    async fn cancel_cluster_update_marks_non_terminal_nodes_cancelled() {
        let (db, svc) = cluster_svc();
        let spec = make_cluster_update_spec();
        let _ = <ControllerService as controller_proto::controller_server::Controller>::create_cluster_update(
            &svc,
            Request::new(controller_proto::CreateClusterUpdateRequest { spec: Some(spec) }),
        )
        .await
        .expect("create");
        // Pretend node-1 already activated; node-2 is still preparing.
        let mut node_rows = db.list_cluster_update_nodes("release-0-3-0").unwrap();
        for n in &mut node_rows {
            if n.node_id == "node-1" {
                n.phase = "succeeded".into();
                db.upsert_cluster_update_node(n).unwrap();
            } else {
                n.phase = "prepared".into();
                db.upsert_cluster_update_node(n).unwrap();
            }
        }
        let _ = <ControllerService as controller_proto::controller_server::Controller>::cancel_cluster_update(
            &svc,
            Request::new(controller_proto::CancelClusterUpdateRequest { name: "release-0-3-0".into() }),
        )
        .await
        .expect("cancel");
        let row = db.get_cluster_update("release-0-3-0").unwrap().unwrap();
        assert_eq!(row.phase, "cancelled");
        let nodes = db.list_cluster_update_nodes("release-0-3-0").unwrap();
        let by_id: std::collections::HashMap<_, _> = nodes
            .iter()
            .map(|n| (n.node_id.clone(), n.phase.clone()))
            .collect();
        assert_eq!(
            by_id["node-1"], "succeeded",
            "succeeded nodes must not be cancelled"
        );
        assert_eq!(by_id["node-2"], "cancelled");
    }

    #[tokio::test]
    async fn rollback_cluster_update_marks_succeeded_and_inflight_nodes_for_rollback() {
        let (db, svc) = cluster_svc();
        let spec = make_cluster_update_spec();
        let _ = <ControllerService as controller_proto::controller_server::Controller>::create_cluster_update(
            &svc,
            Request::new(controller_proto::CreateClusterUpdateRequest { spec: Some(spec) }),
        )
        .await
        .expect("create");
        let mut nodes = db.list_cluster_update_nodes("release-0-3-0").unwrap();
        for n in &mut nodes {
            n.phase = if n.node_id == "node-1" {
                "succeeded".into()
            } else {
                "prepared".into()
            };
            db.upsert_cluster_update_node(n).unwrap();
        }
        let _ = <ControllerService as controller_proto::controller_server::Controller>::rollback_cluster_update(
            &svc,
            Request::new(controller_proto::RollbackClusterUpdateRequest { name: "release-0-3-0".into() }),
        )
        .await
        .expect("rollback");
        let row = db.get_cluster_update("release-0-3-0").unwrap().unwrap();
        assert_eq!(row.phase, "rolling_back");
        for n in db.list_cluster_update_nodes("release-0-3-0").unwrap() {
            assert_eq!(
                n.phase, "rolling_back",
                "all eligible nodes must be flagged for rollback (got node {} phase {})",
                n.node_id, n.phase
            );
        }
    }

    #[tokio::test]
    async fn list_cluster_updates_returns_each_persisted_row() {
        let (_db, svc) = cluster_svc();
        let spec = make_cluster_update_spec();
        let _ = <ControllerService as controller_proto::controller_server::Controller>::create_cluster_update(
            &svc,
            Request::new(controller_proto::CreateClusterUpdateRequest { spec: Some(spec) }),
        )
        .await
        .expect("create");
        let resp = <ControllerService as controller_proto::controller_server::Controller>::list_cluster_updates(
            &svc,
            Request::new(controller_proto::ListClusterUpdatesRequest {}),
        )
        .await
        .expect("list")
        .into_inner();
        assert_eq!(resp.cluster_updates.len(), 1);
    }

    #[tokio::test]
    async fn get_cluster_update_returns_node_statuses() {
        let (db, svc) = cluster_svc();
        let spec = make_cluster_update_spec();
        let _ = <ControllerService as controller_proto::controller_server::Controller>::create_cluster_update(
            &svc,
            Request::new(controller_proto::CreateClusterUpdateRequest { spec: Some(spec) }),
        )
        .await
        .expect("create");
        let mut nodes = db.list_cluster_update_nodes("release-0-3-0").unwrap();
        nodes[0].phase = "prepared".into();
        nodes[0].prepared_closure = "/var/lib/kcore/updates/release-0-3-0/manifest.json".into();
        db.upsert_cluster_update_node(&nodes[0]).unwrap();

        let resp = <ControllerService as controller_proto::controller_server::Controller>::get_cluster_update(
            &svc,
            Request::new(controller_proto::GetClusterUpdateRequest { name: "release-0-3-0".into() }),
        )
        .await
        .expect("get")
        .into_inner();
        assert_eq!(resp.node_statuses.len(), 2);
        let prepared = resp
            .node_statuses
            .iter()
            .find(|n| n.phase == controller_proto::NodeUpdatePhase::Prepared as i32);
        assert!(prepared.is_some(), "expected one node in Prepared phase");
    }

    #[tokio::test]
    async fn plan_cluster_update_returns_target_node_ids() {
        let (_db, svc) = cluster_svc();
        let spec = make_cluster_update_spec();
        let resp = <ControllerService as controller_proto::controller_server::Controller>::plan_cluster_update(
            &svc,
            Request::new(controller_proto::PlanClusterUpdateRequest { spec: Some(spec) }),
        )
        .await
        .expect("plan")
        .into_inner();
        assert!(resp.viable);
        let mut ids = resp.target_node_ids.clone();
        ids.sort();
        assert_eq!(ids, vec!["node-1".to_string(), "node-2".to_string()]);
        assert!(resp.issues.is_empty());
    }

    fn make_ceph_spec(node_ids: &[&str]) -> controller_proto::CephClusterSpec {
        controller_proto::CephClusterSpec {
            fsid: String::new(),
            public_network: "10.10.0.0/24".into(),
            cluster_network: "10.20.0.0/24".into(),
            size: 3,
            min_size: 2,
            force_wipe: false,
            encrypt_osds: true,
            nodes: node_ids
                .iter()
                .enumerate()
                .map(|(i, id)| controller_proto::CephClusterNodeSpec {
                    node_id: (*id).into(),
                    mon_addr: format!("10.10.0.{}:6789", 11 + i),
                    cluster_addr: format!("10.20.0.{}", 11 + i),
                    public_iface: "eth1".into(),
                    cluster_iface: "eth2".into(),
                    osd_device: "/dev/nvme0n1".into(),
                    osd_devices: vec![],
                })
                .collect(),
        }
    }

    #[tokio::test]
    async fn create_ceph_cluster_creates_then_unchanged_then_updated() {
        let (db, svc) = cluster_svc();
        let mut spec = make_ceph_spec(&["node-1", "node-2"]);
        spec.fsid = "11111111-2222-3333-4444-555555555555".into();

        let created = <ControllerService as controller_proto::controller_server::Controller>::create_ceph_cluster(
            &svc,
            Request::new(controller_proto::CreateCephClusterRequest {
                ceph_cluster: Some(controller_proto::CephCluster {
                    name: "lab".into(),
                    generation: 0,
                    spec: Some(spec.clone()),
                    status: None,
                    created_at: None,
                    updated_at: None,
                }),
            }),
        )
        .await
        .expect("create")
        .into_inner();
        assert_eq!(
            created.action,
            controller_proto::ApplyAction::Created as i32
        );
        let cluster = created.ceph_cluster.expect("cluster");
        assert_eq!(cluster.generation, 1);
        assert_eq!(
            cluster.spec.as_ref().unwrap().fsid,
            "11111111-2222-3333-4444-555555555555"
        );
        let status = cluster.status.expect("status");
        assert_eq!(
            status.phase,
            controller_proto::CephClusterPhase::Pending as i32
        );
        assert!(db
            .list_ceph_clusters_needing_reconcile()
            .unwrap()
            .iter()
            .any(|c| c.name == "lab"));

        let same = <ControllerService as controller_proto::controller_server::Controller>::create_ceph_cluster(
            &svc,
            Request::new(controller_proto::CreateCephClusterRequest {
                ceph_cluster: Some(controller_proto::CephCluster {
                    name: "lab".into(),
                    generation: 0,
                    spec: Some(spec.clone()),
                    status: None,
                    created_at: None,
                    updated_at: None,
                }),
            }),
        )
        .await
        .expect("unchanged")
        .into_inner();
        assert_eq!(same.action, controller_proto::ApplyAction::Unchanged as i32);
        assert!(same.changed_fields.is_empty());

        spec.force_wipe = true;
        let updated = <ControllerService as controller_proto::controller_server::Controller>::create_ceph_cluster(
            &svc,
            Request::new(controller_proto::CreateCephClusterRequest {
                ceph_cluster: Some(controller_proto::CephCluster {
                    name: "lab".into(),
                    generation: 0,
                    spec: Some(spec),
                    status: None,
                    created_at: None,
                    updated_at: None,
                }),
            }),
        )
        .await
        .expect("updated")
        .into_inner();
        assert_eq!(
            updated.action,
            controller_proto::ApplyAction::Updated as i32
        );
        assert_eq!(updated.changed_fields, vec!["spec".to_string()]);
        assert_eq!(updated.ceph_cluster.unwrap().generation, 2);
    }

    #[tokio::test]
    async fn create_ceph_cluster_auto_generates_and_preserves_fsid() {
        let (_db, svc) = cluster_svc();
        let mut spec = make_ceph_spec(&["node-1"]);
        spec.fsid.clear();

        let created = <ControllerService as controller_proto::controller_server::Controller>::create_ceph_cluster(
            &svc,
            Request::new(controller_proto::CreateCephClusterRequest {
                ceph_cluster: Some(controller_proto::CephCluster {
                    name: "auto-fsid".into(),
                    generation: 0,
                    spec: Some(spec.clone()),
                    status: None,
                    created_at: None,
                    updated_at: None,
                }),
            }),
        )
        .await
        .expect("create")
        .into_inner();
        let fsid = created
            .ceph_cluster
            .as_ref()
            .and_then(|c| c.spec.as_ref())
            .map(|s| s.fsid.clone())
            .expect("fsid");
        assert!(!fsid.is_empty(), "empty fsid must be auto-generated");
        assert!(Uuid::parse_str(&fsid).is_ok());

        spec.force_wipe = true;
        spec.fsid.clear();
        let updated = <ControllerService as controller_proto::controller_server::Controller>::create_ceph_cluster(
            &svc,
            Request::new(controller_proto::CreateCephClusterRequest {
                ceph_cluster: Some(controller_proto::CephCluster {
                    name: "auto-fsid".into(),
                    generation: 0,
                    spec: Some(spec),
                    status: None,
                    created_at: None,
                    updated_at: None,
                }),
            }),
        )
        .await
        .expect("update")
        .into_inner();
        assert_eq!(
            updated.ceph_cluster.unwrap().spec.unwrap().fsid,
            fsid,
            "re-apply with empty fsid must preserve prior fsid"
        );
    }

    #[tokio::test]
    async fn create_ceph_cluster_rejects_unregistered_node() {
        let (_db, svc) = cluster_svc();
        let spec = make_ceph_spec(&["node-1", "ghost-node"]);
        let err = <ControllerService as controller_proto::controller_server::Controller>::create_ceph_cluster(
            &svc,
            Request::new(controller_proto::CreateCephClusterRequest {
                ceph_cluster: Some(controller_proto::CephCluster {
                    name: "bad".into(),
                    generation: 0,
                    spec: Some(spec),
                    status: None,
                    created_at: None,
                    updated_at: None,
                }),
            }),
        )
        .await
        .expect_err("unknown node");
        assert_eq!(err.code(), tonic::Code::NotFound);
        assert!(err.message().contains("ghost-node"));
    }

    #[tokio::test]
    async fn get_list_delete_ceph_cluster() {
        let (_db, svc) = cluster_svc();
        let mut spec = make_ceph_spec(&["node-1"]);
        spec.fsid = "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee".into();
        <ControllerService as controller_proto::controller_server::Controller>::create_ceph_cluster(
            &svc,
            Request::new(controller_proto::CreateCephClusterRequest {
                ceph_cluster: Some(controller_proto::CephCluster {
                    name: "lab".into(),
                    generation: 0,
                    spec: Some(spec),
                    status: None,
                    created_at: None,
                    updated_at: None,
                }),
            }),
        )
        .await
        .expect("create");

        let got = <ControllerService as controller_proto::controller_server::Controller>::get_ceph_cluster(
            &svc,
            Request::new(controller_proto::GetCephClusterRequest {
                name: "lab".into(),
            }),
        )
        .await
        .expect("get")
        .into_inner();
        assert_eq!(got.ceph_cluster.unwrap().name, "lab");

        let missing = <ControllerService as controller_proto::controller_server::Controller>::get_ceph_cluster(
            &svc,
            Request::new(controller_proto::GetCephClusterRequest {
                name: "nope".into(),
            }),
        )
        .await
        .expect_err("missing");
        assert_eq!(missing.code(), tonic::Code::NotFound);

        let listed = <ControllerService as controller_proto::controller_server::Controller>::list_ceph_clusters(
            &svc,
            Request::new(controller_proto::ListCephClustersRequest {}),
        )
        .await
        .expect("list")
        .into_inner();
        assert_eq!(listed.ceph_clusters.len(), 1);

        let deleted = <ControllerService as controller_proto::controller_server::Controller>::delete_ceph_cluster(
            &svc,
            Request::new(controller_proto::DeleteCephClusterRequest {
                name: "lab".into(),
                force: true,
            }),
        )
        .await
        .expect("delete")
        .into_inner();
        assert!(deleted.success);

        let again = <ControllerService as controller_proto::controller_server::Controller>::delete_ceph_cluster(
            &svc,
            Request::new(controller_proto::DeleteCephClusterRequest {
                name: "lab".into(),
                force: false,
            }),
        )
        .await
        .expect("second delete")
        .into_inner();
        assert!(!again.success);

        let empty = <ControllerService as controller_proto::controller_server::Controller>::list_ceph_clusters(
            &svc,
            Request::new(controller_proto::ListCephClustersRequest {}),
        )
        .await
        .expect("list empty")
        .into_inner();
        assert!(empty.ceph_clusters.is_empty());
    }

    #[test]
    fn node_supports_backend_treats_ceph_cluster_members_as_eligible() {
        let (db, svc) = cluster_svc();
        let mut node = test_node();
        node.storage_backend = "filesystem".into();
        assert!(
            !svc.node_supports_backend(&node, "ceph"),
            "filesystem-only node must not support ceph without membership"
        );

        let mut spec = make_ceph_spec(&["node-1"]);
        spec.fsid = "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee".into();
        let spec_json = crate::ceph_cluster_spec::spec_to_json(&spec).unwrap();
        db.upsert_ceph_cluster(&CephClusterRow {
            name: "lab".into(),
            generation: 1,
            spec_json,
            bootstrap_json: String::new(),
            created_at: String::new(),
            updated_at: String::new(),
        })
        .unwrap();

        assert!(svc.ceph_member_ids().contains("node-1"));
        assert!(
            svc.node_supports_backend(&node, "ceph"),
            "CephCluster member must schedule ceph VMs even if node.storage_backend is filesystem"
        );
        assert!(!svc.node_supports_backend(&node, "zfs"));
    }

    fn postgresql_service() -> (Database, ControllerService, NodeRow) {
        let db = Database::open(":memory:").expect("open db");
        let node = test_node();
        db.upsert_node(&node).expect("insert node");
        let hook: PushHook = Arc::new(|_n: &NodeRow| Ok(()));
        let svc = ControllerService::new_with_test_push_hook(
            db.clone(),
            NodeClients::new(None),
            test_network(),
            None,
            false,
            hook,
        );
        (db, svc, node)
    }

    #[tokio::test]
    async fn create_postgresql_is_idempotent_and_one_database_per_node() {
        let (db, svc, node) = postgresql_service();
        let created = <ControllerService as controller_proto::controller_server::Controller>::create_postgresql(
            &svc,
            Request::new(controller_proto::CreatePostgresqlRequest {
                name: "app".into(),
                database: String::new(),
                package: String::new(),
                port: 0,
                target_node: node.id.clone(),
            }),
        )
        .await
        .expect("create")
        .into_inner();
        assert_eq!(
            created.action,
            controller_proto::ApplyAction::Created as i32
        );
        let stored = db.get_postgresql("app").unwrap().expect("row");
        assert_eq!(stored.database_name, "app");
        assert_eq!(stored.package, "postgresql");
        assert_eq!(stored.port, 5432);
        assert_eq!(stored.node_id, node.id);

        let again = <ControllerService as controller_proto::controller_server::Controller>::create_postgresql(
            &svc,
            Request::new(controller_proto::CreatePostgresqlRequest {
                name: "app".into(),
                database: "app".into(),
                package: "postgresql".into(),
                port: 5432,
                target_node: String::new(),
            }),
        )
        .await
        .expect("unchanged")
        .into_inner();
        assert_eq!(
            again.action,
            controller_proto::ApplyAction::Unchanged as i32
        );

        let updated = <ControllerService as controller_proto::controller_server::Controller>::create_postgresql(
            &svc,
            Request::new(controller_proto::CreatePostgresqlRequest {
                name: "app".into(),
                database: "app".into(),
                package: "postgresql".into(),
                port: 5433,
                target_node: node.id.clone(),
            }),
        )
        .await
        .expect("port update")
        .into_inner();
        assert_eq!(
            updated.action,
            controller_proto::ApplyAction::Updated as i32
        );
        assert_eq!(updated.changed_fields, vec!["port".to_string()]);

        let package_change = <ControllerService as controller_proto::controller_server::Controller>::create_postgresql(
            &svc,
            Request::new(controller_proto::CreatePostgresqlRequest {
                name: "app".into(),
                database: "app".into(),
                package: "postgresql_16".into(),
                port: 5433,
                target_node: node.id.clone(),
            }),
        )
        .await;
        assert!(package_change.is_err());

        let second = <ControllerService as controller_proto::controller_server::Controller>::create_postgresql(
            &svc,
            Request::new(controller_proto::CreatePostgresqlRequest {
                name: "other".into(),
                database: "other".into(),
                package: "postgresql".into(),
                port: 5432,
                target_node: node.id.clone(),
            }),
        )
        .await;
        assert!(second.is_err());

        let deleted = <ControllerService as controller_proto::controller_server::Controller>::delete_postgresql(
            &svc,
            Request::new(controller_proto::DeletePostgresqlRequest {
                name: "app".into(),
            }),
        )
        .await
        .expect("delete")
        .into_inner();
        assert!(deleted.success);
        assert!(db.get_postgresql("app").unwrap().is_none());
    }

    fn web_request<T>(operator: &str, msg: T) -> Request<T> {
        let mut req = Request::new(msg);
        req.extensions_mut().insert(auth::WebIdentity {
            operator: operator.to_string(),
            token_id: format!("tok-{operator}"),
            kind: auth::WebTokenKind::Session,
            max_role: None,
            expires_at: i64::MAX,
        });
        req
    }

    #[tokio::test]
    async fn web_console_input_reaches_only_the_owners_session() {
        let db = Database::open(":memory:").expect("open db");
        for name in ["alice", "bob"] {
            db.create_operator(name).unwrap();
            db.grant_operator_role_str(name, "vm-admin").unwrap();
        }
        let svc = ControllerService::new(
            db,
            NodeClients::new(None),
            test_network(),
            empty_sub_ca(),
            None,
            false,
            false,
        );
        let (to_node, mut node_rx) = mpsc::channel(4);
        svc.web_consoles.0.lock().unwrap().insert(
            "s1".to_string(),
            WebConsole {
                owner: "kctl:alice".to_string(),
                to_node,
            },
        );
        let input = |operator: &str, data: Vec<u8>| {
            web_request(
                operator,
                controller_proto::SendVmConsoleInputRequest {
                    session_id: "s1".to_string(),
                    data,
                },
            )
        };
        use controller_proto::controller_server::Controller as _;

        let err = svc
            .send_vm_console_input(input("bob", b"ls\n".to_vec()))
            .await
            .unwrap_err();
        assert_eq!(err.code(), tonic::Code::NotFound);
        assert!(node_rx.try_recv().is_err());

        let err = svc
            .send_vm_console_input(input("alice", vec![0; WEB_CONSOLE_MAX_INPUT + 1]))
            .await
            .unwrap_err();
        assert_eq!(err.code(), tonic::Code::InvalidArgument);

        svc.send_vm_console_input(input("alice", b"ls\n".to_vec()))
            .await
            .expect("owner input");
        assert_eq!(node_rx.recv().await.unwrap().data, b"ls\n");

        drop(node_rx);
        let err = svc
            .send_vm_console_input(input("alice", b"x".to_vec()))
            .await
            .unwrap_err();
        assert_eq!(err.code(), tonic::Code::NotFound);
    }
    #[tokio::test]
    async fn uncordon_refuses_slurm_compute_member_without_force() {
        let (db, svc, node) = postgresql_service();
        db.upsert_slurm_cluster(&SlurmClusterRow {
            name: "hpc-a".into(),
            spec_json: "{}".into(),
            generation: 1,
            munge_key: vec![1; 1024],
            dbd_password: String::new(),
            status_json: "{}".into(),
            created_at: String::new(),
            updated_at: String::new(),
        })
        .unwrap();
        db.replace_slurm_cluster_nodes(
            "hpc-a",
            &[SlurmClusterNodeRow {
                node_id: node.id.clone(),
                cluster_name: "hpc-a".into(),
                roles: "compute".into(),
                applied_generation: 1,
                slurm_state: "idle".into(),
                config_hash: String::new(),
                updated_at: String::new(),
            }],
        )
        .unwrap();
        db.update_node_status(&node.id, "cordoned").unwrap();

        let err =
            <ControllerService as controller_proto::controller_server::Controller>::uncordon_node(
                &svc,
                Request::new(controller_proto::UncordonNodeRequest {
                    node_id: node.id.clone(),
                    force: false,
                }),
            )
            .await
            .unwrap_err();
        assert_eq!(err.code(), tonic::Code::FailedPrecondition);
        assert!(err.message().contains("compute member"));

        <ControllerService as controller_proto::controller_server::Controller>::uncordon_node(
            &svc,
            Request::new(controller_proto::UncordonNodeRequest {
                node_id: node.id.clone(),
                force: true,
            }),
        )
        .await
        .expect("force uncordon");
        assert_eq!(db.get_node(&node.id).unwrap().unwrap().status, "ready");
    }
}
