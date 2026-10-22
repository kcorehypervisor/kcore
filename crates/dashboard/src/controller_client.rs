//! gRPC client for the controller (kctl-equivalent transport).

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

use anyhow::{Context, Result};
use tonic::transport::{Certificate, Channel, ClientTlsConfig, Endpoint, Identity};

use crate::config::DashboardConfig;

const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(20);

fn channel_cache() -> &'static Mutex<HashMap<String, Channel>> {
    static CACHE: OnceLock<Mutex<HashMap<String, Channel>>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

fn channel_key(cfg: &DashboardConfig) -> String {
    let domain = cfg.tls_domain.as_deref().unwrap_or("");
    let cert = cfg
        .cert
        .as_ref()
        .map(|path| path.display().to_string())
        .unwrap_or_default();
    format!(
        "{}|{}|{}|{}",
        cfg.insecure, cfg.controller_addr, domain, cert
    )
}

pub mod controller_proto {
    #![allow(clippy::result_large_err, clippy::double_must_use)]
    tonic::include_proto!("kcore.controller");
}

pub fn endpoint_host(address: &str) -> Option<&str> {
    if let Some(rest) = address.strip_prefix('[') {
        if let Some(end_idx) = rest.find(']') {
            return Some(&rest[..end_idx]);
        }
    }
    address
        .rsplit_once(':')
        .map(|(host, _)| host)
        .or(Some(address))
}

pub async fn connect_channel(cfg: &DashboardConfig) -> Result<Channel> {
    // Every dashboard page used to open a new connection. Reuse one HTTP/2
    // channel per controller so navigation does not pay for a handshake again.
    let key = channel_key(cfg);
    if let Some(channel) = channel_cache()
        .lock()
        .unwrap_or_else(|err| err.into_inner())
        .get(&key)
        .cloned()
    {
        return Ok(channel);
    }

    let scheme = if cfg.insecure { "http" } else { "https" };
    let uri = format!("{scheme}://{}", cfg.controller_addr);
    let mut endpoint = Endpoint::from_shared(uri)
        .context("invalid controller URI")?
        .connect_timeout(CONNECT_TIMEOUT)
        .timeout(REQUEST_TIMEOUT)
        .tcp_keepalive(Some(Duration::from_secs(30)))
        .http2_keep_alive_interval(Duration::from_secs(30))
        .keep_alive_timeout(Duration::from_secs(10))
        .keep_alive_while_idle(true);

    if !cfg.insecure {
        let (ca_pem, cert_pem, key_pem) = cfg.tls_pem_strings()?;
        let mut tls = ClientTlsConfig::new();
        tls = tls.ca_certificate(Certificate::from_pem(ca_pem));
        tls = tls.identity(Identity::from_pem(cert_pem, key_pem));
        let verify_name = cfg
            .tls_domain
            .clone()
            .or_else(|| endpoint_host(&cfg.controller_addr).map(str::to_string));
        if let Some(host) = verify_name {
            tls = tls.domain_name(host);
        }
        endpoint = endpoint
            .tls_config(tls)
            .context("TLS config for controller endpoint")?;
    }

    let channel = endpoint.connect().await.with_context(|| {
        format!(
            "gRPC connect to {} failed (check KCORE_CONTROLLER matches a SAN in the controller cert)",
            cfg.controller_addr,
        )
    })?;
    channel_cache()
        .lock()
        .unwrap_or_else(|err| err.into_inner())
        .insert(key, channel.clone());
    Ok(channel)
}

pub async fn get_compliance(
    cfg: &DashboardConfig,
) -> Result<controller_proto::GetComplianceReportResponse> {
    let channel = connect_channel(cfg).await?;
    let mut client = controller_proto::controller_client::ControllerClient::new(channel);
    let resp = client
        .get_compliance_report(controller_proto::GetComplianceReportRequest {})
        .await
        .context("GetComplianceReport RPC")?;
    Ok(resp.into_inner())
}

pub async fn list_vms(cfg: &DashboardConfig) -> Result<Vec<controller_proto::VmInfo>> {
    let channel = connect_channel(cfg).await?;
    let mut client = controller_proto::controller_client::ControllerClient::new(channel);
    let resp = client
        .list_vms(controller_proto::ListVmsRequest {
            target_node: String::new(),
        })
        .await
        .context("ListVms RPC")?;
    Ok(resp.into_inner().vms)
}

pub async fn list_networks(cfg: &DashboardConfig) -> Result<Vec<controller_proto::NetworkInfo>> {
    let channel = connect_channel(cfg).await?;
    let mut client = controller_proto::controller_client::ControllerClient::new(channel);
    let resp = client
        .list_networks(controller_proto::ListNetworksRequest {
            target_node: String::new(),
        })
        .await
        .context("ListNetworks RPC")?;
    Ok(resp.into_inner().networks)
}

pub async fn get_network_overview(
    cfg: &DashboardConfig,
) -> Result<controller_proto::GetNetworkOverviewResponse> {
    let channel = connect_channel(cfg).await?;
    let mut client = controller_proto::controller_client::ControllerClient::new(channel);
    let resp = client
        .get_network_overview(controller_proto::GetNetworkOverviewRequest {})
        .await
        .context("GetNetworkOverview RPC")?;
    Ok(resp.into_inner())
}

pub async fn get_storage_overview(
    cfg: &DashboardConfig,
) -> Result<controller_proto::GetStorageOverviewResponse> {
    let channel = connect_channel(cfg).await?;
    let mut client = controller_proto::controller_client::ControllerClient::new(channel);
    let resp = client
        .get_storage_overview(controller_proto::GetStorageOverviewRequest {})
        .await
        .context("GetStorageOverview RPC")?;
    Ok(resp.into_inner())
}

pub async fn get_replication_status(
    cfg: &DashboardConfig,
) -> Result<controller_proto::GetReplicationStatusResponse> {
    let channel = connect_channel(cfg).await?;
    let mut client = controller_proto::controller_admin_client::ControllerAdminClient::new(channel);
    let resp = client
        .get_replication_status(controller_proto::GetReplicationStatusRequest {})
        .await
        .context("GetReplicationStatus RPC")?;
    Ok(resp.into_inner())
}

pub async fn list_replication_conflicts(
    cfg: &DashboardConfig,
) -> Result<controller_proto::ListReplicationConflictsResponse> {
    let channel = connect_channel(cfg).await?;
    let mut client = controller_proto::controller_admin_client::ControllerAdminClient::new(channel);
    let resp = client
        .list_replication_conflicts(controller_proto::ListReplicationConflictsRequest {
            limit: 100,
        })
        .await
        .context("ListReplicationConflicts RPC")?;
    Ok(resp.into_inner())
}

pub async fn list_volumes(cfg: &DashboardConfig) -> Result<Vec<controller_proto::VolumeInfo>> {
    let channel = connect_channel(cfg).await?;
    let mut client = controller_proto::controller_client::ControllerClient::new(channel);
    let resp = client
        .list_volumes(controller_proto::ListVolumesRequest { vm: String::new() })
        .await
        .context("ListVolumes RPC")?;
    Ok(resp.into_inner().volumes)
}

pub async fn list_ceph_clusters(
    cfg: &DashboardConfig,
) -> Result<Vec<controller_proto::CephCluster>> {
    let channel = connect_channel(cfg).await?;
    let mut client = controller_proto::controller_client::ControllerClient::new(channel);
    let resp = client
        .list_ceph_clusters(controller_proto::ListCephClustersRequest {})
        .await
        .context("ListCephClusters RPC")?;
    Ok(resp.into_inner().ceph_clusters)
}

pub async fn get_ceph_cluster(
    cfg: &DashboardConfig,
    name: &str,
) -> Result<controller_proto::CephCluster> {
    let channel = connect_channel(cfg).await?;
    let mut client = controller_proto::controller_client::ControllerClient::new(channel);
    let resp = client
        .get_ceph_cluster(controller_proto::GetCephClusterRequest {
            name: name.to_string(),
        })
        .await
        .context("GetCephCluster RPC")?;
    resp.into_inner()
        .ceph_cluster
        .ok_or_else(|| anyhow::anyhow!("GetCephCluster returned empty cluster"))
}

pub async fn get_ceph_cluster_capacity(
    cfg: &DashboardConfig,
    name: &str,
) -> Result<controller_proto::GetCephClusterCapacityResponse> {
    let channel = connect_channel(cfg).await?;
    let mut client = controller_proto::controller_client::ControllerClient::new(channel);
    let resp = client
        .get_ceph_cluster_capacity(controller_proto::GetCephClusterCapacityRequest {
            name: name.to_string(),
        })
        .await
        .context("GetCephClusterCapacity RPC")?;
    Ok(resp.into_inner())
}

pub async fn list_shared_filesystems(
    cfg: &DashboardConfig,
) -> Result<Vec<controller_proto::SharedFilesystem>> {
    let channel = connect_channel(cfg).await?;
    let mut client = controller_proto::controller_client::ControllerClient::new(channel);
    let resp = client
        .list_shared_filesystems(controller_proto::ListSharedFilesystemsRequest {})
        .await
        .context("ListSharedFilesystems RPC")?;
    Ok(resp.into_inner().shared_filesystems)
}

pub async fn list_object_stores(
    cfg: &DashboardConfig,
) -> Result<Vec<controller_proto::ObjectStore>> {
    let channel = connect_channel(cfg).await?;
    let mut client = controller_proto::controller_client::ControllerClient::new(channel);
    let resp = client
        .list_object_stores(controller_proto::ListObjectStoresRequest {})
        .await
        .context("ListObjectStores RPC")?;
    Ok(resp.into_inner().object_stores)
}

pub async fn list_vm_operations(
    cfg: &DashboardConfig,
    include_finished: bool,
) -> Result<Vec<controller_proto::VmOperation>> {
    let channel = connect_channel(cfg).await?;
    let mut client = controller_proto::controller_client::ControllerClient::new(channel);
    let resp = client
        .list_vm_operations(controller_proto::ListVmOperationsRequest {
            include_finished,
            vm_id: String::new(),
        })
        .await
        .context("ListVmOperations RPC")?;
    Ok(resp.into_inner().operations)
}

pub async fn list_postgresqls(
    cfg: &DashboardConfig,
) -> Result<Vec<controller_proto::PostgresqlInfo>> {
    let channel = connect_channel(cfg).await?;
    let mut client = controller_proto::controller_client::ControllerClient::new(channel);
    let resp = client
        .list_postgresqls(controller_proto::ListPostgresqlsRequest {
            target_node: String::new(),
        })
        .await
        .context("ListPostgresqls RPC")?;
    Ok(resp.into_inner().postgresqls)
}

pub async fn get_postgresql(
    cfg: &DashboardConfig,
    name: &str,
) -> Result<controller_proto::PostgresqlInfo> {
    let channel = connect_channel(cfg).await?;
    let mut client = controller_proto::controller_client::ControllerClient::new(channel);
    let resp = client
        .get_postgresql(controller_proto::GetPostgresqlRequest {
            name: name.to_string(),
        })
        .await
        .with_context(|| format!("GetPostgresql RPC for '{name}'"))?;
    resp.into_inner()
        .postgresql
        .with_context(|| format!("postgresql '{name}' not found"))
}

pub async fn list_kubernetes_clusters(
    cfg: &DashboardConfig,
) -> Result<Vec<controller_proto::KubernetesCluster>> {
    let channel = connect_channel(cfg).await?;
    let mut client = controller_proto::controller_client::ControllerClient::new(channel);
    let resp = client
        .list_kubernetes_clusters(controller_proto::ListKubernetesClustersRequest {})
        .await
        .context("ListKubernetesClusters RPC")?;
    Ok(resp.into_inner().clusters)
}

pub async fn get_kubernetes_cluster(
    cfg: &DashboardConfig,
    name: &str,
) -> Result<controller_proto::KubernetesCluster> {
    let channel = connect_channel(cfg).await?;
    let mut client = controller_proto::controller_client::ControllerClient::new(channel);
    let resp = client
        .get_kubernetes_cluster(controller_proto::GetKubernetesClusterRequest {
            name: name.to_string(),
        })
        .await
        .with_context(|| format!("GetKubernetesCluster RPC for '{name}'"))?;
    resp.into_inner()
        .cluster
        .with_context(|| format!("kubernetes cluster '{name}' not found"))
}

pub async fn get_kubernetes_cluster_health(
    cfg: &DashboardConfig,
    name: &str,
) -> Result<controller_proto::GetKubernetesClusterHealthResponse> {
    let channel = connect_channel(cfg).await?;
    let mut client = controller_proto::controller_client::ControllerClient::new(channel);
    let resp = client
        .get_kubernetes_cluster_health(controller_proto::GetKubernetesClusterHealthRequest {
            name: name.to_string(),
        })
        .await
        .with_context(|| format!("GetKubernetesClusterHealth RPC for '{name}'"))?;
    Ok(resp.into_inner())
}

pub async fn list_audit_events(
    cfg: &DashboardConfig,
    limit: u32,
    since: &str,
    action: &str,
) -> Result<Vec<controller_proto::AuditEvent>> {
    let channel = connect_channel(cfg).await?;
    let mut client = controller_proto::controller_client::ControllerClient::new(channel);
    let resp = client
        .list_audit_events(controller_proto::ListAuditEventsRequest {
            limit,
            since: since.to_string(),
            action: action.to_string(),
        })
        .await
        .context("ListAuditEvents RPC")?;
    Ok(resp.into_inner().events)
}

async fn controller(
    cfg: &DashboardConfig,
) -> Result<controller_proto::controller_client::ControllerClient<Channel>> {
    let channel = connect_channel(cfg).await?;
    Ok(controller_proto::controller_client::ControllerClient::new(
        channel,
    ))
}

pub async fn get_vm(cfg: &DashboardConfig, name: &str) -> Result<controller_proto::GetVmResponse> {
    let mut client = controller(cfg).await?;
    let resp = client
        .get_vm(controller_proto::GetVmRequest {
            vm_id: name.to_string(),
            target_node: String::new(),
        })
        .await
        .with_context(|| format!("GetVm RPC for '{name}'"))?;
    Ok(resp.into_inner())
}

pub async fn get_live_migrate_receive_status(
    cfg: &DashboardConfig,
    name: &str,
) -> Result<controller_proto::GetLiveMigrateReceiveStatusResponse> {
    let mut client = controller(cfg).await?;
    let resp = client
        .get_live_migrate_receive_status(controller_proto::GetLiveMigrateReceiveStatusRequest {
            vm_id: name.to_string(),
            node_id: String::new(),
        })
        .await
        .with_context(|| format!("GetLiveMigrateReceiveStatus RPC for '{name}'"))?;
    Ok(resp.into_inner())
}

pub async fn get_volume(cfg: &DashboardConfig, name: &str) -> Result<controller_proto::VolumeInfo> {
    let mut client = controller(cfg).await?;
    let resp = client
        .get_volume(controller_proto::GetVolumeRequest {
            name: name.to_string(),
        })
        .await
        .with_context(|| format!("GetVolume RPC for '{name}'"))?;
    resp.into_inner()
        .volume
        .with_context(|| format!("volume '{name}' not found"))
}

pub async fn list_volume_snapshots(
    cfg: &DashboardConfig,
    volume: &str,
) -> Result<Vec<controller_proto::VolumeSnapshotInfo>> {
    let mut client = controller(cfg).await?;
    let resp = client
        .list_volume_snapshots(controller_proto::ListVolumeSnapshotsRequest {
            volume: volume.to_string(),
        })
        .await
        .context("ListVolumeSnapshots RPC")?;
    Ok(resp.into_inner().snapshots)
}

pub async fn list_snapshot_policies(
    cfg: &DashboardConfig,
) -> Result<Vec<controller_proto::SnapshotPolicy>> {
    let mut client = controller(cfg).await?;
    let resp = client
        .list_snapshot_policies(controller_proto::ListSnapshotPoliciesRequest {})
        .await
        .context("ListSnapshotPolicies RPC")?;
    Ok(resp.into_inner().policies)
}

pub async fn get_snapshot_policy(
    cfg: &DashboardConfig,
    name: &str,
) -> Result<controller_proto::SnapshotPolicy> {
    let mut client = controller(cfg).await?;
    let resp = client
        .get_snapshot_policy(controller_proto::GetSnapshotPolicyRequest {
            name: name.to_string(),
        })
        .await
        .with_context(|| format!("GetSnapshotPolicy RPC for '{name}'"))?;
    resp.into_inner()
        .policy
        .with_context(|| format!("snapshot policy '{name}' not found"))
}

pub async fn get_shared_filesystem(
    cfg: &DashboardConfig,
    name: &str,
) -> Result<controller_proto::SharedFilesystem> {
    let mut client = controller(cfg).await?;
    let resp = client
        .get_shared_filesystem(controller_proto::GetSharedFilesystemRequest {
            name: name.to_string(),
        })
        .await
        .with_context(|| format!("GetSharedFilesystem RPC for '{name}'"))?;
    resp.into_inner()
        .shared_filesystem
        .with_context(|| format!("shared filesystem '{name}' not found"))
}

pub async fn get_object_store(
    cfg: &DashboardConfig,
    name: &str,
) -> Result<controller_proto::ObjectStore> {
    let mut client = controller(cfg).await?;
    let resp = client
        .get_object_store(controller_proto::GetObjectStoreRequest {
            name: name.to_string(),
        })
        .await
        .with_context(|| format!("GetObjectStore RPC for '{name}'"))?;
    resp.into_inner()
        .object_store
        .with_context(|| format!("object store '{name}' not found"))
}

pub async fn get_vm_operation(
    cfg: &DashboardConfig,
    id: &str,
) -> Result<controller_proto::VmOperation> {
    let mut client = controller(cfg).await?;
    let resp = client
        .get_vm_operation(controller_proto::GetVmOperationRequest { id: id.to_string() })
        .await
        .with_context(|| format!("GetVmOperation RPC for '{id}'"))?;
    resp.into_inner()
        .operation
        .with_context(|| format!("operation '{id}' not found"))
}

pub async fn list_nodes(cfg: &DashboardConfig) -> Result<Vec<controller_proto::NodeInfo>> {
    let mut client = controller(cfg).await?;
    let resp = client
        .list_nodes(controller_proto::ListNodesRequest {})
        .await
        .context("ListNodes RPC")?;
    Ok(resp.into_inner().nodes)
}

pub async fn get_node(cfg: &DashboardConfig, id: &str) -> Result<controller_proto::NodeInfo> {
    let mut client = controller(cfg).await?;
    let resp = client
        .get_node(controller_proto::GetNodeRequest {
            node_id: id.to_string(),
        })
        .await
        .with_context(|| format!("GetNode RPC for '{id}'"))?;
    resp.into_inner()
        .node
        .with_context(|| format!("node '{id}' not found"))
}

pub async fn list_pci_devices(
    cfg: &DashboardConfig,
    node_id: &str,
) -> Result<Vec<controller_proto::GpuInfo>> {
    let mut client = controller(cfg).await?;
    let resp = client
        .list_pci_devices(controller_proto::ListGpusRequest {
            node_id: node_id.to_string(),
        })
        .await
        .context("ListPciDevices RPC")?;
    Ok(resp.into_inner().gpus)
}

pub async fn list_containers(
    cfg: &DashboardConfig,
) -> Result<Vec<controller_proto::ContainerInfo>> {
    let mut client = controller(cfg).await?;
    let resp = client
        .list_workloads(controller_proto::ListWorkloadsRequest {
            kind: controller_proto::WorkloadKind::Container as i32,
            target_node: String::new(),
        })
        .await
        .context("ListWorkloads RPC")?;
    Ok(resp.into_inner().containers)
}

pub async fn get_container(
    cfg: &DashboardConfig,
    name: &str,
    node: &str,
) -> Result<controller_proto::GetWorkloadResponse> {
    let mut client = controller(cfg).await?;
    let resp = client
        .get_workload(controller_proto::GetWorkloadRequest {
            kind: controller_proto::WorkloadKind::Container as i32,
            workload_id: name.to_string(),
            target_node: node.to_string(),
        })
        .await
        .with_context(|| format!("GetWorkload RPC for '{name}'"))?;
    Ok(resp.into_inner())
}

pub async fn list_disk_layouts(
    cfg: &DashboardConfig,
) -> Result<Vec<controller_proto::DiskLayoutSummary>> {
    let mut client = controller(cfg).await?;
    let resp = client
        .list_disk_layouts(controller_proto::ListDiskLayoutsRequest {
            node_id: String::new(),
        })
        .await
        .context("ListDiskLayouts RPC")?;
    Ok(resp.into_inner().disk_layouts)
}

pub async fn get_disk_layout(
    cfg: &DashboardConfig,
    name: &str,
) -> Result<controller_proto::GetDiskLayoutResponse> {
    let mut client = controller(cfg).await?;
    let resp = client
        .get_disk_layout(controller_proto::GetDiskLayoutRequest {
            name: name.to_string(),
        })
        .await
        .with_context(|| format!("GetDiskLayout RPC for '{name}'"))?;
    Ok(resp.into_inner())
}

pub async fn get_cluster_health(
    cfg: &DashboardConfig,
) -> Result<controller_proto::GetClusterHealthResponse> {
    let mut client = controller(cfg).await?;
    let resp = client
        .get_cluster_health(controller_proto::GetClusterHealthRequest {})
        .await
        .context("GetClusterHealth RPC")?;
    Ok(resp.into_inner())
}

pub async fn list_certificates(
    cfg: &DashboardConfig,
) -> Result<Vec<controller_proto::CertificateInfo>> {
    let mut client = controller(cfg).await?;
    let resp = client
        .list_certificates(controller_proto::ListCertificatesRequest::default())
        .await
        .context("ListCertificates RPC")?;
    Ok(resp.into_inner().certificates)
}

pub async fn get_pki_status(
    cfg: &DashboardConfig,
) -> Result<controller_proto::GetPkiStatusResponse> {
    let mut client = controller(cfg).await?;
    let resp = client
        .get_pki_status(controller_proto::GetPkiStatusRequest {})
        .await
        .context("GetPkiStatus RPC")?;
    Ok(resp.into_inner())
}

pub async fn get_crl(cfg: &DashboardConfig) -> Result<controller_proto::GetCrlResponse> {
    let mut client = controller(cfg).await?;
    let resp = client
        .get_crl(controller_proto::GetCrlRequest {})
        .await
        .context("GetCrl RPC")?;
    Ok(resp.into_inner())
}

pub async fn list_ssh_keys(cfg: &DashboardConfig) -> Result<Vec<controller_proto::SshKeyInfo>> {
    let mut client = controller(cfg).await?;
    let resp = client
        .list_ssh_keys(controller_proto::ListSshKeysRequest {})
        .await
        .context("ListSshKeys RPC")?;
    Ok(resp.into_inner().keys)
}

pub async fn get_ssh_key(
    cfg: &DashboardConfig,
    name: &str,
) -> Result<controller_proto::SshKeyInfo> {
    let mut client = controller(cfg).await?;
    let resp = client
        .get_ssh_key(controller_proto::GetSshKeyRequest {
            name: name.to_string(),
        })
        .await
        .with_context(|| format!("GetSshKey RPC for '{name}'"))?;
    resp.into_inner()
        .key
        .with_context(|| format!("ssh key '{name}' not found"))
}

pub async fn export_sbom(
    cfg: &DashboardConfig,
    kind: &str,
) -> Result<controller_proto::ExportSbomResponse> {
    let mut client = controller(cfg).await?;
    let resp = client
        .export_sbom(controller_proto::ExportSbomRequest {
            kind: kind.to_string(),
        })
        .await
        .with_context(|| format!("ExportSbom RPC for '{kind}'"))?;
    Ok(resp.into_inner())
}

pub async fn get_crypto_config(
    cfg: &DashboardConfig,
) -> Result<controller_proto::GetCryptoConfigResponse> {
    let mut client = controller(cfg).await?;
    let resp = client
        .get_crypto_config(controller_proto::GetCryptoConfigRequest {})
        .await
        .context("GetCryptoConfig RPC")?;
    Ok(resp.into_inner())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn endpoint_host_parsing() {
        assert_eq!(endpoint_host("10.0.0.1:9090"), Some("10.0.0.1"));
        assert_eq!(endpoint_host("[::1]:9090"), Some("::1"));
    }
}
