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
    DiskLayoutRow, DiskLayoutStatusRow, NetworkRow, NodeRow, OperatorRow, SecurityGroupRow,
    SecurityGroupRuleRow, VmRow, VolumeRow, WorkloadRow,
};
use crate::node_proto;
use crate::{nixgen, node_client::NodeClients, scheduler};
use kcore_sanitize::sanitize_nix_attr_key;
use std::collections::HashMap;

use super::helpers::compute_vni;
use super::helpers::{
    controller_state_from_node_state, grpc_address_host, migration_dial_host,
    parse_datetime_to_timestamp, parse_port_list, short_vm_id_seed, state_fallback_without_runtime,
    status_with_context, vm_backend_handle,
};
use super::rbac_matrix;
use super::signing;
use super::validation::{
    derive_image_format, derive_image_format_from_path, derive_local_image_path,
    normalize_image_format, normalize_storage_backend, storage_backend_to_proto,
    validate_image_path, validate_image_sha256, validate_image_url, validate_ipv4,
    validate_netmask, validate_network_name, validate_network_type, validate_storage_size_bytes,
};

#[cfg(test)]
type PushHook = std::sync::Arc<dyn Fn(&NodeRow) -> Result<(), Status> + Send + Sync + 'static>;

/// Live migrate failed; `send_succeeded` means the guest may already run on dest
/// and cold fallback must not start a second VMM on the shared RBD.
struct LiveMigrateFailure {
    send_succeeded: bool,
    status: Status,
}

/// Whether a node may be handed a VM that is moving off another node.
///
/// `scheduler::select_node_for_vm` applies these rules when it picks a target,
/// but an operator-supplied `target_node` used to bypass them entirely — so
/// `MigrateVm` and `DrainNode` would happily move a guest onto a node that was
/// itself being evacuated, or one that has not been approved into the cluster.
#[allow(clippy::result_large_err)]
fn accepts_migrated_vms(node: &NodeRow) -> Result<(), Status> {
    if node.approval_status != "approved" {
        return Err(Status::failed_precondition(format!(
            "node '{}' is not approved ({}); it cannot be given VMs",
            node.id, node.approval_status
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
const EVT_SECURITY_GROUP_CREATE: &str = "security_group.create";
const EVT_SECURITY_GROUP_DELETE: &str = "security_group.delete";
const EVT_SECURITY_GROUP_ATTACH: &str = "security_group.attach";
const EVT_SECURITY_GROUP_DETACH: &str = "security_group.detach";
const EVT_NODE_DRAIN: &str = "node.drain";
const EVT_VM_MIGRATE: &str = "vm.migrate";
const EVT_SSH_KEY_CREATE: &str = "ssh_key.create";
const EVT_SSH_KEY_DELETE: &str = "ssh_key.delete";
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

pub struct ControllerService {
    db: Database,
    clients: NodeClients,
    default_network: NetworkConfig,
    sub_ca: Arc<Mutex<SubCaState>>,
    replication: Option<ReplicationConfig>,
    tls_paths: Option<TlsPaths>,
    require_manual_approval: bool,
    /// When true, legacy `CN=kctl` keeps cluster-admin after operators exist (escape hatch).
    bootstrap_kctl: bool,
    pki: PkiRuntime,
    #[cfg(test)]
    test_push_hook: Option<PushHook>,
}

impl ControllerService {
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
            #[cfg(test)]
            test_push_hook: None,
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
            test_push_hook: Some(hook),
        }
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
        self.push_config_to_node_inner(node, false).await
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
        self.push_config_to_node_inner(node, true).await
    }

    async fn push_config_to_node_inner(
        &self,
        node: &NodeRow,
        await_apply: bool,
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
        let networks = self
            .db
            .list_networks_for_node(&node.id)
            .map_err(|e| Status::internal(format!("listing networks: {e}")))?;

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

        let mut vxlan_peers: std::collections::HashMap<String, nixgen::VxlanMeta> =
            std::collections::HashMap::new();
        for net in &networks {
            if net.network_type == "vxlan" {
                let all_with_name = self
                    .db
                    .list_networks_by_name(&net.name)
                    .map_err(|e| Status::internal(format!("listing vxlan peers: {e}")))?;
                let peers: Vec<String> = all_with_name
                    .iter()
                    .filter(|n| n.node_id != node.id)
                    .filter_map(|n| {
                        self.db
                            .get_node(&n.node_id)
                            .ok()
                            .flatten()
                            .map(|nd| nd.address.split(':').next().unwrap_or("").to_string())
                    })
                    .filter(|ip| !ip.is_empty())
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

        let nix_config = nixgen::generate_node_config_with_security_groups(
            &vms,
            iface,
            &self.default_network,
            &networks,
            &vm_ssh_keys,
            &vxlan_peers,
            &security_group_rules,
        );

        let mut admin = self.ensure_admin_client_for_node(node).await?;

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

        let available_cpu = node.cpu_cores - node.cpu_used;
        let available_memory = node.memory_bytes - node.memory_used;
        if available_cpu < spec.cpu || available_memory < spec.memory_bytes {
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
                    && (n.cpu_cores - n.cpu_used) >= cpu
                    && (n.memory_bytes - n.memory_used) >= memory_bytes
            })
            .collect()
    }

    pub(crate) fn ceph_member_ids(&self) -> HashSet<String> {
        self.db
            .list_ceph_clusters()
            .unwrap_or_default()
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

    async fn live_migrate_vm(
        &self,
        vm: &VmRow,
        source: &NodeRow,
        target: &NodeRow,
        volume: &VolumeRow,
        runtime_name: &str,
        dest_host: &str,
    ) -> Result<(), LiveMigrateFailure> {
        let mut dest_admin = self
            .ensure_admin_client_for_node(target)
            .await
            .map_err(|status| LiveMigrateFailure {
                send_succeeded: false,
                status,
            })?;
        let mut source_admin =
            self.ensure_admin_client_for_node(source)
                .await
                .map_err(|status| LiveMigrateFailure {
                    send_succeeded: false,
                    status,
                })?;

        let prep = dest_admin
            .prepare_live_migrate_receive(node_proto::PrepareLiveMigrateReceiveRequest {
                vm_name: runtime_name.to_string(),
                rbd_pool: volume.pool.clone(),
                rbd_image: volume.image.clone(),
                listen_port: 0,
            })
            .await
            .map_err(|e| LiveMigrateFailure {
                send_succeeded: false,
                status: status_with_context(
                    &e,
                    &format!("preparing node {} to receive VM '{}'", target.id, vm.name),
                ),
            })?
            .into_inner();
        if !prep.success || prep.listen_port <= 0 {
            return Err(LiveMigrateFailure {
                send_succeeded: false,
                status: Status::internal(format!("prepare receive failed: {}", prep.message)),
            });
        }
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
                    rbd_pool: volume.pool.clone(),
                    rbd_image: volume.image.clone(),
                })
                .await;
            return Err(LiveMigrateFailure {
                send_succeeded: false,
                status: status_with_context(
                    &e,
                    &format!(
                        "sending VM '{}' from node {} to {destination_url}",
                        vm.name, source.id
                    ),
                ),
            });
        }

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
            .map_err(|e| LiveMigrateFailure {
                send_succeeded: true,
                status: status_with_context(
                    &e,
                    &format!(
                        "waiting for VM '{}' to finish arriving on node {}",
                        vm.name, target.id
                    ),
                ),
            })?
            .into_inner();
        if !wait.success {
            return Err(LiveMigrateFailure {
                send_succeeded: true,
                status: Status::internal(format!(
                    "node {} did not complete the receive for VM '{}': {}",
                    target.id, vm.name, wait.message
                )),
            });
        }

        self.reassign_vm_node(vm, &target.id)
            .map_err(|status| LiveMigrateFailure {
                send_succeeded: true,
                status,
            })?;

        if let Err(e) = self.push_config_to_node(source).await {
            warn!(node = %source.id, error = %e, "push after live migrate (source)");
        }
        // `finalize_live_migrate_dest` starts the generated VM unit, so the
        // destination rebuild has to have activated before we ask for it.
        self.push_config_and_await_apply(target)
            .await
            .map_err(|status| LiveMigrateFailure {
                send_succeeded: true,
                status,
            })?;

        dest_admin
            .finalize_live_migrate_dest(node_proto::FinalizeLiveMigrateDestRequest {
                vm_name: runtime_name.to_string(),
            })
            .await
            .map_err(|e| LiveMigrateFailure {
                send_succeeded: true,
                status: status_with_context(
                    &e,
                    &format!(
                        "adopting migrated VM '{}' into systemd on node {}",
                        vm.name, target.id
                    ),
                ),
            })?;

        // The guest already runs on the destination, so a source that has not
        // fully let go is a leak to clean up later, not a reason to fail the
        // migration — but it must be visible.
        match source_admin
            .finalize_live_migrate_source(node_proto::FinalizeLiveMigrateSourceRequest {
                vm_name: runtime_name.to_string(),
                rbd_pool: volume.pool.clone(),
                rbd_image: volume.image.clone(),
            })
            .await
        {
            Ok(resp) => {
                let resp = resp.into_inner();
                if !resp.vmm_stopped || !resp.rbd_unmapped {
                    warn!(
                        node = %source.id,
                        vm = %vm.name,
                        vmm_stopped = resp.vmm_stopped,
                        rbd_unmapped = resp.rbd_unmapped,
                        message = %resp.message,
                        "source node did not fully release the migrated VM"
                    );
                }
            }
            Err(e) => {
                warn!(node = %source.id, error = %e, "finalize source after live migrate");
            }
        }

        Ok(())
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
        let volume = self
            .db
            .get_volume_by_vm(&vm.id)
            .map_err(|e| Status::internal(e.to_string()))?;
        let (pool, image) = match volume {
            Some(v) => (v.pool, v.image),
            None => {
                // Without an image name the node cannot report `rbd unmapped`
                // for anything specific, so the barrier degrades to "the VM
                // unit is stopped" — which does still run the unit's
                // `ExecStopPost` unmap. Say so rather than hide it.
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
        let diff = crate::grpc::diff::diff_vm(&stored, &stored_ssh, &apply);

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

    fn audit_actor<T>(request: &Request<T>) -> String {
        if let Some(actor) = AUDIT_ACTOR_OVERRIDE.with(|c| c.borrow().clone()) {
            return actor;
        }
        auth::peer_cn(request).unwrap_or_else(|| "insecure".to_string())
    }

    fn push_audit_actor(actor: &str) -> AuditActorGuard {
        AUDIT_ACTOR_OVERRIDE.with(|c| {
            *c.borrow_mut() = Some(actor.to_string());
        });
        AuditActorGuard
    }

    fn record_audit(&self, actor: &str, action: &str, resource: &str, detail: impl Into<String>) {
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

    fn log_replication_event_required(
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

        let (action, changed_fields, generation) = if let Some(existing) = existing.as_ref() {
            if existing.node_id != node_id {
                return Err(Status::invalid_argument(format!(
                    "cannot change immutable field(s) on disk layout '{name}': node_id \
                     (delete the disk layout and recreate)"
                )));
            }
            if existing.layout_nix == layout_nix {
                (
                    controller_proto::ApplyAction::Unchanged as i32,
                    Vec::<String>::new(),
                    existing.generation,
                )
            } else {
                (
                    controller_proto::ApplyAction::Updated as i32,
                    vec!["layout_nix".to_string()],
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
        _ => controller_proto::NodeUpdatePhase::Unspecified as i32,
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

#[tonic::async_trait]
impl controller_proto::controller_server::Controller for ControllerService {
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

        let node = NodeRow {
            id: req.node_id.clone(),
            hostname: req.hostname.clone(),
            address: req.address.clone(),
            cpu_cores: cpu,
            memory_bytes: mem,
            status: if approval_status == "approved" {
                "ready".into()
            } else {
                "pending".into()
            },
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
                "status": if approval_status == "approved" { "ready" } else { "pending" },
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

        if let Ok(Some(node)) = self.db.get_node(&req.node_id) {
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

        Ok(Response::new(controller_proto::HeartbeatResponse {
            success: true,
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
            match self
                .db
                .update_vm_runtime_state(&req.node_id, &vm.name, state_str)
            {
                Ok(true) => {}
                Ok(false) => {
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
        self.require_operator(&request, OperatorRole::VmAdmin)?;
        let actor = Self::audit_actor(&request);
        let mut req = request.into_inner();
        let spec = req
            .spec
            .take()
            .ok_or_else(|| Status::invalid_argument("spec is required"))?;

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

        let target_node_requested = !req.target_node.is_empty();
        let mut node = if target_node_requested {
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
            let compatible_nodes: Vec<NodeRow> = nodes
                .into_iter()
                .filter(|n| self.node_supports_backend(n, &requested_storage_backend))
                .collect();
            let target_dc = req.target_dc.trim();
            if target_dc.is_empty() {
                scheduler::select_node_for_vm(&compatible_nodes, spec.cpu, spec.memory_bytes)
            } else {
                scheduler::select_node_for_vm_in_dc(
                    &compatible_nodes,
                    spec.cpu,
                    spec.memory_bytes,
                    target_dc,
                )
            }
            .cloned()
            .ok_or_else(|| {
                if target_dc.is_empty() {
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
                if let Some(fallback) = scheduler::select_node_for_vm(
                    &self.alternative_vm_create_nodes(
                        &node.id,
                        &requested_storage_backend,
                        spec.cpu,
                        spec.memory_bytes,
                    ),
                    spec.cpu,
                    spec.memory_bytes,
                )
                .cloned()
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
        if image_url_input.is_empty() && image_path_input.is_empty() {
            return Err(Status::invalid_argument(
                "either image_url or image_path is required",
            ));
        }
        if !image_url_input.is_empty() && !image_path_input.is_empty() {
            return Err(Status::invalid_argument(
                "image_url and image_path are mutually exclusive",
            ));
        }

        let (image_url, image_sha256, image_path, image_format) = if !image_url_input.is_empty() {
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
        if let Some(conflict) = existing_on_node
            .into_iter()
            .find(|existing| existing.image_path == image_path)
        {
            return Err(Status::failed_precondition(format!(
                "image path '{}' is already used by VM '{}' on node '{}'; duplicate writable disk usage is not supported",
                image_path, conflict.name, node.id
            )));
        }
        let vm_network = spec
            .nics
            .first()
            .map(|n| n.network.clone())
            .unwrap_or_else(|| "default".into());
        if vm_network != "default"
            && self
                .db
                .get_network_for_node(&node.id, &vm_network)
                .map_err(|e| Status::internal(format!("checking network: {e}")))?
                .is_none()
        {
            return Err(Status::failed_precondition(format!(
                "network '{}' is not configured on node '{}'",
                vm_network, node.id
            )));
        }

        let vm_ip = if vm_network == "default" {
            self.reserve_nat_vm_ip_for_network(
                &node.id,
                &vm_network,
                &vm_id,
                &self.default_network.gateway_ip,
            )?
        } else if let Some(net) = self
            .db
            .get_network_for_node(&node.id, &vm_network)
            .map_err(|e| Status::internal(format!("fetching network: {e}")))?
        {
            match net.network_type.as_str() {
                "vxlan" => self
                    .db
                    .allocate_vm_ip_global(&vm_network)
                    .map_err(|e| Status::internal(format!("allocating VM IP: {e}")))?,
                "nat" => self.reserve_nat_vm_ip_for_network(
                    &node.id,
                    &vm_network,
                    &vm_id,
                    &net.gateway_ip,
                )?,
                _ => String::new(),
            }
        } else {
            String::new()
        };

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
        };

        if vm.storage_backend == "ceph" {
            if !self.is_healthy_ceph_member(&node.id)? {
                return Err(Status::failed_precondition(
                    "storage_backend ceph requires a healthy CephCluster that includes the target node",
                ));
            }
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
            storage
                .create_volume(node_proto::CreateVolumeRequest {
                    volume_id: format!("kcore-{}", vm.id),
                    storage_class: "ceph".into(),
                    size_bytes: vm.storage_size_bytes,
                    parameters: HashMap::new(),
                })
                .await
                .map_err(|e| Status::internal(format!("creating RBD volume: {e}")))?;
        }

        if let Err(e) = self.db.insert_vm(&vm) {
            if vm.storage_backend == "ceph" {
                self.rollback_created_vm(&node, &vm).await;
            }
            return Err(Status::internal(format!("storing vm: {e}")));
        }
        if vm.storage_backend == "ceph" {
            if let Err(e) = self.db.upsert_volume(&VolumeRow {
                id: Uuid::new_v4().to_string(),
                vm_id: vm.id.clone(),
                pool: "kcore-vms".into(),
                image: format!("kcore-{}", vm.id),
                size_bytes: vm.storage_size_bytes,
                created_at: String::new(),
            }) {
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
                "sshKeyNames": ssh_key_names,
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

        if cpu.is_none() && mem.is_none() {
            return Err(Status::invalid_argument(
                "at least one of cpu or memory_bytes must be set",
            ));
        }

        let updated = self
            .db
            .update_vm_spec(&req.vm_id, cpu, mem)
            .map_err(|e| Status::internal(format!("updating vm: {e}")))?;
        if !updated {
            return Err(Status::not_found(format!("VM '{}' not found", req.vm_id)));
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
        self.require_operator(&request, OperatorRole::VmAdmin)?;
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
        let volume = self
            .db
            .get_volume_by_vm(&db_vm.id)
            .map_err(|e| Status::internal(e.to_string()))?;

        // `rbd rm` cannot remove an image the owning node still has mapped, and
        // the config push that stops the guest only happens at the end of this
        // RPC. Deleting the volume first therefore left the RBD image behind in
        // the pool with its bookkeeping row already gone. Stop the guest and
        // unmap first; an unreachable node is tolerated so a dead host can
        // still be cleaned up.
        self.cold_release_ceph_vm(&db_vm, &node).await?;

        let deleted = self
            .db
            .delete_vm_by_id_or_name(&db_vm.id)
            .map_err(|e| Status::internal(format!("deleting vm: {e}")))?;
        if !deleted {
            return Err(Status::not_found(format!("VM '{}' not found", req.vm_id)));
        }
        if let Some(vol) = volume {
            if self.clients.get_storage(&node.address).is_none() {
                let _ = self.clients.connect(&node.address).await;
            }
            if let Some(mut storage) = self.clients.get_storage(&node.address) {
                let handle = format!("{}/{}", vol.pool, vol.image);
                if let Err(e) = storage
                    .delete_volume(node_proto::DeleteVolumeRequest {
                        backend_handle: handle,
                    })
                    .await
                {
                    warn!(vm_id = %db_vm.id, error = %e, "failed to delete RBD volume");
                }
            }
            self.db
                .delete_volume_by_vm(&db_vm.id)
                .map_err(|e| Status::internal(format!("deleting volume row: {e}")))?;
        }

        info!(vm_id = %db_vm.id, node_id = %node.id, "deleted VM, pushing config");

        self.push_config_to_node(&node).await?;
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
        self.require_operator(&request, OperatorRole::VmAdmin)?;
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
                    nics: vec![controller_proto::Nic {
                        network: db_vm.network.clone(),
                        model: "virtio".to_string(),
                        mac_address: String::new(),
                    }],
                    storage_backend: db_vm.storage_backend.clone(),
                    storage_size_bytes: db_vm.storage_size_bytes,
                    desired_state: if db_vm.auto_start {
                        controller_proto::VmDesiredState::Running as i32
                    } else {
                        controller_proto::VmDesiredState::Stopped as i32
                    },
                });
                let status = Some(controller_proto::VmStatus {
                    id: db_vm.id.clone(),
                    state: state_fallback_without_runtime(db_vm.auto_start),
                    created_at: None,
                    updated_at: None,
                });
                return Ok(Response::new(controller_proto::GetVmResponse {
                    spec,
                    status,
                    node_id: node.id,
                    assigned_ip: db_vm.vm_ip.clone(),
                }));
            }
        };

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
                nics = vec![controller_proto::Nic {
                    network: db_vm.network.clone(),
                    model: "virtio".to_string(),
                    mac_address: nics
                        .first()
                        .map(|n| n.mac_address.clone())
                        .unwrap_or_default(),
                }];
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
            }
        });

        let status = inner.status.map(|s| controller_proto::VmStatus {
            id: s.id,
            state: controller_state_from_node_state(s.state),
            created_at: s.created_at,
            updated_at: s.updated_at,
        });

        Ok(Response::new(controller_proto::GetVmResponse {
            spec,
            status,
            node_id: node.id,
            assigned_ip: db_vm.vm_ip,
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
        let vm_key = first.vm_name.trim();
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
                data: first.data,
            })
            .await
            .map_err(|_| Status::unavailable("failed to open console stream to node"))?;

        let node_outbound = ReceiverStream::new(to_node_rx);
        let mut from_node = admin
            .attach_vm_console(node_outbound)
            .await
            .map_err(|e| Status::unavailable(format!("node AttachVmConsole: {e}")))?
            .into_inner();

        // Session established: who opened which console, and when (created_at).
        self.record_audit(
            &actor,
            "AttachVmConsole",
            &format!("vm/{vm_name}"),
            serde_json::json!({ "nodeId": node.id }).to_string(),
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

        let vm_count = rows.len();
        let mut fallback_states: Vec<i32> = Vec::with_capacity(vm_count);
        let mut set = tokio::task::JoinSet::new();

        for (idx, vm) in rows.iter().enumerate() {
            fallback_states.push(state_fallback_without_runtime(vm.auto_start));
            if let Some(node_address) = node_address_by_id.get(&vm.node_id) {
                if self.clients.get_compute(node_address).is_none() {
                    if let Err(err) = self.clients.connect(node_address).await {
                        warn!(address = %node_address, error = %err, "failed to refresh node compute client");
                    }
                }
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

        let infos: Vec<_> = rows
            .into_iter()
            .enumerate()
            .map(|(i, vm)| {
                let state = live_states[i].unwrap_or(fallback_states[i]);
                controller_proto::VmInfo {
                    id: vm.id,
                    name: vm.name,
                    state,
                    cpu: vm.cpu,
                    memory_bytes: vm.memory_bytes,
                    node_id: vm.node_id,
                    created_at: None,
                    storage_backend: vm.storage_backend,
                    storage_size_bytes: vm.storage_size_bytes,
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

        Ok(Response::new(controller_proto::ListNetworksResponse {
            networks: rows
                .into_iter()
                .map(|n| controller_proto::NetworkInfo {
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
                })
                .collect(),
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

        let source_node = self
            .db
            .get_node(&req.node_id)
            .map_err(|e| Status::internal(e.to_string()))?
            .ok_or_else(|| Status::not_found(format!("node '{}' not found", req.node_id)))?;

        self.db
            .update_node_status(&req.node_id, "draining")
            .map_err(|e| Status::internal(format!("updating node status: {e}")))?;

        let vms = self
            .db
            .list_vms_for_node(&req.node_id)
            .map_err(|e| Status::internal(format!("listing vms: {e}")))?;

        if vms.is_empty() {
            self.db
                .update_node_status(&req.node_id, "drained")
                .map_err(|e| Status::internal(format!("updating node status: {e}")))?;
            return Ok(Response::new(controller_proto::DrainNodeResponse {
                success: true,
                vms_migrated: 0,
                message: "node has no VMs, marked as drained".into(),
            }));
        }

        let all_nodes = self
            .db
            .list_nodes()
            .map_err(|e| Status::internal(e.to_string()))?;

        let mut migrated = 0i32;
        let mut errors = Vec::new();
        let eligible_nodes: Vec<NodeRow> = all_nodes
            .iter()
            .filter(|n| n.id != req.node_id)
            .cloned()
            .collect();

        let mut destination_node_ids: std::collections::HashSet<String> =
            std::collections::HashSet::new();

        for vm in &vms {
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
            // Every failure below has to be recorded and skipped rather than
            // returned: VMs earlier in the loop have already been reassigned in
            // the DB and no config has been pushed yet, so bailing out here
            // would leave the cluster disagreeing with the database.
            let target = if !req.target_node.is_empty() {
                match backend_eligible
                    .iter()
                    .find(|n| n.id == req.target_node || n.address == req.target_node)
                {
                    Some(n) => n,
                    None => {
                        errors.push(format!(
                            "target node '{}' cannot take VM '{}' (storage backend '{}')",
                            req.target_node, vm.name, vm.storage_backend
                        ));
                        continue;
                    }
                }
            } else {
                match scheduler::select_node_for_vm(&backend_eligible, vm.cpu, vm.memory_bytes) {
                    Some(n) => n,
                    None => {
                        errors.push(format!(
                            "no node with capacity and compatible storage for VM '{}' ({})",
                            vm.name, vm.storage_backend
                        ));
                        continue;
                    }
                }
            };

            // Stop the guest and unmap the shared RBD on the source before the
            // destination is allowed to map it. Skipping a VM here is far
            // better than two nodes writing the same image.
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

        // A node is only drained once its own rebuild has actually removed the
        // VM units, so wait for the verdict and treat a failure as an
        // incomplete evacuation rather than logging it and claiming success.
        if let Err(e) = self.push_config_and_await_apply(&source_node).await {
            warn!(node = %req.node_id, error = %e, "failed to apply config on drained node");
            errors.push(format!(
                "node {} did not apply its post-drain configuration: {e}",
                req.node_id
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

        // Only claim the node is drained when nothing was left running on it;
        // otherwise it stays `draining` so operators (and the reconciler) can
        // see the evacuation is incomplete.
        let final_status = if errors.is_empty() {
            "drained"
        } else {
            "draining"
        };
        self.db
            .update_node_status(&req.node_id, final_status)
            .map_err(|e| Status::internal(format!("updating node status: {e}")))?;

        let msg = if errors.is_empty() {
            format!("{migrated} VMs migrated successfully")
        } else {
            format!(
                "{migrated} VMs migrated, {} errors: {}",
                errors.len(),
                errors.join("; ")
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
                "migrated": migrated,
                "errors": errors,
            }),
        )?;

        Ok(Response::new(controller_proto::DrainNodeResponse {
            success: errors.is_empty(),
            vms_migrated: migrated,
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

        if vm.storage_backend != "ceph" {
            return Err(Status::failed_precondition(
                "live migrate requires storage_backend=ceph (shared RBD)",
            ));
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
                &volume,
                &runtime_name,
                &dest_host,
            )
            .await
        {
            Ok(()) => {
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
                    }),
                )?;
                return Ok(Response::new(controller_proto::MigrateVmResponse {
                    success: true,
                    message: format!(
                        "live-migrated '{}' from {} to {}",
                        vm.name, source_node.id, target.id
                    ),
                    mode: "live".into(),
                    source_node: source_node.id,
                    target_node: target.id.clone(),
                }));
            }
            Err(live_err) => {
                warn!(
                    vm = %vm.name,
                    send_succeeded = live_err.send_succeeded,
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
                }))
            }
        }
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
        let mut backend_unspecified_nodes: i32 = 0;
        for n in &approved {
            match n.storage_backend.as_str() {
                "filesystem" => backend_filesystem_nodes += 1,
                "lvm" => backend_lvm_nodes += 1,
                "zfs" => backend_zfs_nodes += 1,
                _ => backend_unspecified_nodes += 1,
            }
        }

        let mut nodes_out = Vec::with_capacity(approved.len());
        let mut nodes_disk_inventory_ok: i32 = 0;
        let mut total_block_devices: i32 = 0;

        for node in &approved {
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
        let _ = request.into_inner();

        let vms = self
            .db
            .list_vms()
            .map_err(|e| Status::internal(e.to_string()))?;
        let ceph_volumes: HashMap<String, VolumeRow> = self
            .db
            .list_volumes()
            .map_err(|e| Status::internal(e.to_string()))?
            .into_iter()
            .map(|v| (v.vm_id.clone(), v))
            .collect();

        let node_address_by_id: std::collections::HashMap<String, String> = self
            .db
            .list_nodes()
            .map_err(|e| Status::internal(e.to_string()))?
            .into_iter()
            .map(|n| (n.id, n.address))
            .collect();

        let vm_count = vms.len();
        let mut fallback_states: Vec<i32> = Vec::with_capacity(vm_count);
        let mut set = tokio::task::JoinSet::new();

        for (idx, vm) in vms.iter().enumerate() {
            fallback_states.push(state_fallback_without_runtime(vm.auto_start));
            if let Some(node_address) = node_address_by_id.get(&vm.node_id) {
                if self.clients.get_compute(node_address).is_none() {
                    let _ = self.clients.connect(node_address).await;
                }
                if let Some(mut compute) = self.clients.get_compute(node_address) {
                    let vm_name = vm.name.clone();
                    set.spawn(async move {
                        let result = tokio::time::timeout(
                            Duration::from_secs(3),
                            compute.get_vm(node_proto::GetVmRequest {
                                vm_id: vm_name.clone(),
                            }),
                        )
                        .await;
                        (idx, result)
                    });
                }
            }
        }

        let mut live_states: Vec<Option<i32>> = vec![None; vm_count];
        while let Some(Ok((idx, result))) = set.join_next().await {
            if let Ok(Ok(resp)) = result {
                if let Some(status) = resp.into_inner().status {
                    live_states[idx] = Some(controller_state_from_node_state(status.state));
                }
            }
        }

        let volumes: Vec<_> = vms
            .into_iter()
            .enumerate()
            .map(|(i, vm)| {
                let state = live_states[i].unwrap_or(fallback_states[i]);
                let ceph = ceph_volumes.get(&vm.id);
                controller_proto::VolumeInfo {
                    vm_id: vm.id.clone(),
                    vm_name: vm.name.clone(),
                    node_id: vm.node_id.clone(),
                    storage_backend: vm.storage_backend.clone(),
                    storage_size_bytes: ceph.map(|v| v.size_bytes).unwrap_or(vm.storage_size_bytes),
                    backend_handle: ceph
                        .map(|v| format!("/dev/rbd/{}/{}", v.pool, v.image))
                        .unwrap_or_else(|| vm_backend_handle(&vm)),
                    image_format: if vm.storage_backend == "lvm"
                        || vm.storage_backend == "zfs"
                        || vm.storage_backend == "ceph"
                    {
                        "raw".to_string()
                    } else {
                        vm.image_format.clone()
                    },
                    vm_state: state,
                }
            })
            .collect();

        Ok(Response::new(controller_proto::ListVolumesResponse {
            volumes,
        }))
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
                }
            })
            .collect();

        let access_control = rbac_matrix::compliance_access_control_entries();

        Ok(Response::new(
            controller_proto::GetComplianceReportResponse {
                controller_version: env!("CARGO_PKG_VERSION").to_string(),
                crypto_library: "aws-lc-rs (AWS-LC, FIPS 140-3 #4816)".into(),
                tls13_cipher_suites: vec![
                    "TLS_AES_256_GCM_SHA384".into(),
                    "TLS_AES_128_GCM_SHA256".into(),
                ],
                tls12_cipher_suites: vec![
                    "TLS_ECDHE_ECDSA_WITH_AES_256_GCM_SHA384".into(),
                    "TLS_ECDHE_ECDSA_WITH_AES_128_GCM_SHA256".into(),
                    "TLS_ECDHE_RSA_WITH_AES_256_GCM_SHA384".into(),
                    "TLS_ECDHE_RSA_WITH_AES_128_GCM_SHA256".into(),
                ],
                kx_groups: vec!["secp384r1 (P-384)".into(), "secp256r1 (P-256)".into()],
                excluded_algorithms: vec![
                    "ChaCha20-Poly1305".into(),
                    "X25519".into(),
                    "RSA key exchange".into(),
                ],
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

    async fn delete_ceph_cluster(
        &self,
        request: Request<controller_proto::DeleteCephClusterRequest>,
    ) -> Result<Response<controller_proto::DeleteCephClusterResponse>, Status> {
        self.require_operator(&request, OperatorRole::ClusterAdmin)?;
        let name = request.into_inner().name;
        let name = name.trim();
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
        let success = self
            .db
            .delete_ceph_cluster(name)
            .map_err(|e| Status::internal(e.to_string()))?;
        Ok(Response::new(controller_proto::DeleteCephClusterResponse {
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
            }
        }
        let viable = !target_ids.is_empty() && issues.is_empty();
        Ok(Response::new(controller_proto::PlanClusterUpdateResponse {
            viable,
            target_node_ids: target_ids,
            issues,
            likely_requires_reboot: true,
            detail: "MVP: assumes flake rollout may restart kcore services".into(),
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
            if n.phase == "succeeded" || n.phase == "prepared" || n.phase == "pending" {
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

    async fn classify_disk_layout(
        &self,
        request: Request<controller_proto::ClassifyDiskLayoutRequest>,
    ) -> Result<Response<controller_proto::ClassifyDiskLayoutResponse>, Status> {
        self.require_operator(&request, OperatorRole::ReadOnly)?;
        self.classify_disk_layout_impl(request.into_inner()).await
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
        candidate.address = "127.0.0.2:9091".to_string();
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
        candidate.address = "127.0.0.2:9091".to_string();
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
        node_b.address = "127.0.0.2:9091".to_string();
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
        node_b.address = "127.0.0.2:9091".into();
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
                }),
            )
            .await
            .expect_err("non-ceph should fail");
        assert_eq!(err.code(), tonic::Code::FailedPrecondition);
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
    fn ceph_two_node_fixture() -> (Database, VmRow) {
        let db = Database::open(":memory:").expect("open db");
        let mut node_a = test_node();
        node_a.id = "node-a".into();
        db.upsert_node(&node_a).unwrap();
        let mut node_b = test_node();
        node_b.id = "node-b".into();
        node_b.address = "127.0.0.2:9091".into();
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
        db.upsert_volume(&VolumeRow {
            id: "vol-ceph-1".into(),
            vm_id: vm.id.clone(),
            pool: "kcore-vms".into(),
            image: format!("kcore-{}", vm.id),
            size_bytes: 8 * 1024 * 1024 * 1024,
            created_at: String::new(),
        })
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

    #[tokio::test]
    async fn delete_ceph_cluster_refuses_while_ceph_vms_still_use_it() {
        let (db, vm) = ceph_two_node_fixture();
        let svc = svc_for(&db);
        let err = <ControllerService as controller_proto::controller_server::Controller>::delete_ceph_cluster(
            &svc,
            Request::new(controller_proto::DeleteCephClusterRequest { name: "lab".into() }),
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
            Request::new(controller_proto::DeleteCephClusterRequest { name: "lab".into() }),
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
            Request::new(controller_proto::DeleteCephClusterRequest { name: "lab".into() }),
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
        node_b.address = "127.0.0.2:9091".into();
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
        db.upsert_volume(&VolumeRow {
            id: "vol-1".into(),
            vm_id: vm.id.clone(),
            pool: "kcore-vms".into(),
            image: format!("kcore-{}", vm.id),
            size_bytes: 8 * 1024 * 1024 * 1024,
            created_at: String::new(),
        })
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
        db.upsert_volume(&VolumeRow {
            id: "vol-del".into(),
            vm_id: vm.id.clone(),
            pool: "kcore-vms".into(),
            image: format!("kcore-{}", vm.id),
            size_bytes: 1024,
            created_at: String::new(),
        })
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
        db.upsert_volume(&VolumeRow {
            id: "vol-rb".into(),
            vm_id: vm.id.clone(),
            pool: "kcore-vms".into(),
            image: format!("kcore-{}", vm.id),
            size_bytes: 1024,
            created_at: String::new(),
        })
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
                    address: "127.0.0.1:9091".to_string(),
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
    /// node *address* (e.g. `127.0.0.1:9091`) used to trip the immutable
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
}
