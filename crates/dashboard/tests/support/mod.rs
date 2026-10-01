//! Shared mock controller for HTTP integration tests.
#![allow(dead_code)]

use std::net::SocketAddr;

use kcore_dashboard::controller_client::controller_proto::controller_admin_server::ControllerAdmin;
use kcore_dashboard::controller_client::controller_proto::controller_server::Controller;
use kcore_dashboard::controller_client::controller_proto::{
    controller_admin_server::ControllerAdminServer, controller_server::ControllerServer, *,
};
use tokio::net::TcpListener;
use tokio_stream::wrappers::TcpListenerStream;
use tonic::transport::Server;
use tonic::{Request, Response, Status};

#[derive(Clone, Default)]
pub struct MockController;

fn unimp(name: &'static str) -> Status {
    Status::unimplemented(name)
}

#[tonic::async_trait]
impl Controller for MockController {
    async fn register_node(
        &self,
        _: Request<RegisterNodeRequest>,
    ) -> Result<Response<RegisterNodeResponse>, Status> {
        Err(unimp("register_node"))
    }

    async fn heartbeat(
        &self,
        _: Request<HeartbeatRequest>,
    ) -> Result<Response<HeartbeatResponse>, Status> {
        Err(unimp("heartbeat"))
    }

    async fn sync_vm_state(
        &self,
        _: Request<SyncVmStateRequest>,
    ) -> Result<Response<SyncVmStateResponse>, Status> {
        Err(unimp("sync_vm_state"))
    }

    async fn sync_workload_state(
        &self,
        _: Request<SyncWorkloadStateRequest>,
    ) -> Result<Response<SyncWorkloadStateResponse>, Status> {
        Err(unimp("sync_workload_state"))
    }

    async fn create_vm(
        &self,
        _: Request<CreateVmRequest>,
    ) -> Result<Response<CreateVmResponse>, Status> {
        Err(unimp("create_vm"))
    }

    async fn update_vm(
        &self,
        _: Request<UpdateVmRequest>,
    ) -> Result<Response<UpdateVmResponse>, Status> {
        Err(unimp("update_vm"))
    }

    async fn delete_vm(
        &self,
        _: Request<DeleteVmRequest>,
    ) -> Result<Response<DeleteVmResponse>, Status> {
        Err(unimp("delete_vm"))
    }

    async fn set_vm_desired_state(
        &self,
        _: Request<SetVmDesiredStateRequest>,
    ) -> Result<Response<SetVmDesiredStateResponse>, Status> {
        Err(unimp("set_vm_desired_state"))
    }

    async fn get_vm(&self, _: Request<GetVmRequest>) -> Result<Response<GetVmResponse>, Status> {
        Err(unimp("get_vm"))
    }

    async fn list_gpus(
        &self,
        _: Request<ListGpusRequest>,
    ) -> Result<Response<ListGpusResponse>, Status> {
        Err(unimp("list_gpus"))
    }

    async fn list_pci_devices(
        &self,
        _: Request<ListGpusRequest>,
    ) -> Result<Response<ListGpusResponse>, Status> {
        Err(unimp("list_pci_devices"))
    }

    type AttachVmConsoleStream =
        tokio_stream::wrappers::ReceiverStream<Result<ConsoleMessage, Status>>;

    async fn attach_vm_console(
        &self,
        request: Request<tonic::Streaming<ConsoleMessage>>,
    ) -> Result<Response<Self::AttachVmConsoleStream>, Status> {
        let mut inbound = request.into_inner();
        let first = inbound
            .message()
            .await?
            .ok_or_else(|| Status::invalid_argument("empty"))?;
        let (tx, rx) = tokio::sync::mpsc::channel(16);
        // Echo first payload (if any) then echo subsequent client data.
        if !first.data.is_empty() {
            let _ = tx
                .send(Ok(ConsoleMessage {
                    vm_name: String::new(),
                    data: first.data.clone(),
                }))
                .await;
        }
        tokio::spawn(async move {
            while let Ok(Some(msg)) = inbound.message().await {
                if tx
                    .send(Ok(ConsoleMessage {
                        vm_name: String::new(),
                        data: msg.data,
                    }))
                    .await
                    .is_err()
                {
                    break;
                }
            }
        });
        Ok(Response::new(tokio_stream::wrappers::ReceiverStream::new(
            rx,
        )))
    }

    async fn list_vms(
        &self,
        _: Request<ListVmsRequest>,
    ) -> Result<Response<ListVmsResponse>, Status> {
        Ok(Response::new(ListVmsResponse {
            vms: vec![VmInfo {
                id: "vm-mock-1".into(),
                name: "mock-vm-alpha".into(),
                state: VmState::Running as i32,
                cpu: 2,
                memory_bytes: 512 * 1024 * 1024,
                node_id: "node-mock-a".into(),
                created_at: None,
                storage_backend: "lvm".into(),
                storage_size_bytes: 0,
            }],
        }))
    }

    async fn create_workload(
        &self,
        _: Request<CreateWorkloadRequest>,
    ) -> Result<Response<CreateWorkloadResponse>, Status> {
        Err(unimp("create_workload"))
    }

    async fn delete_workload(
        &self,
        _: Request<DeleteWorkloadRequest>,
    ) -> Result<Response<DeleteWorkloadResponse>, Status> {
        Err(unimp("delete_workload"))
    }

    async fn set_workload_desired_state(
        &self,
        _: Request<SetWorkloadDesiredStateRequest>,
    ) -> Result<Response<SetWorkloadDesiredStateResponse>, Status> {
        Err(unimp("set_workload_desired_state"))
    }

    async fn get_workload(
        &self,
        _: Request<GetWorkloadRequest>,
    ) -> Result<Response<GetWorkloadResponse>, Status> {
        Err(unimp("get_workload"))
    }

    async fn list_workloads(
        &self,
        _: Request<ListWorkloadsRequest>,
    ) -> Result<Response<ListWorkloadsResponse>, Status> {
        Err(unimp("list_workloads"))
    }

    async fn create_network(
        &self,
        _: Request<CreateNetworkRequest>,
    ) -> Result<Response<CreateNetworkResponse>, Status> {
        Err(unimp("create_network"))
    }

    async fn delete_network(
        &self,
        _: Request<DeleteNetworkRequest>,
    ) -> Result<Response<DeleteNetworkResponse>, Status> {
        Err(unimp("delete_network"))
    }

    async fn list_networks(
        &self,
        _: Request<ListNetworksRequest>,
    ) -> Result<Response<ListNetworksResponse>, Status> {
        Ok(Response::new(ListNetworksResponse {
            networks: vec![
                NetworkInfo {
                    name: "mock-net-stub".into(),
                    external_ip: "203.0.113.1".into(),
                    gateway_ip: "10.0.0.1".into(),
                    internal_netmask: "255.255.255.0".into(),
                    node_id: "node-mock-a".into(),
                    allowed_tcp_ports: vec![],
                    allowed_udp_ports: vec![],
                    vlan_id: 0,
                    network_type: "nat".into(),
                    enable_outbound_nat: true,
                    ..Default::default()
                },
                NetworkInfo {
                    name: "mock-overlay".into(),
                    external_ip: String::new(),
                    gateway_ip: "10.50.0.1".into(),
                    internal_netmask: "255.255.255.0".into(),
                    node_id: "node-mock-a".into(),
                    allowed_tcp_ports: vec![22],
                    allowed_udp_ports: vec![],
                    vlan_id: 0,
                    network_type: "vxlan".into(),
                    enable_outbound_nat: false,
                    ipv6_prefix: "fd00:10::/64".into(),
                    ipv6_gateway: "fd00:10::1".into(),
                    east_west_firewall: true,
                },
            ],
        }))
    }

    async fn create_postgresql(
        &self,
        _: Request<CreatePostgresqlRequest>,
    ) -> Result<Response<CreatePostgresqlResponse>, Status> {
        Err(unimp("create_postgresql"))
    }

    async fn delete_postgresql(
        &self,
        _: Request<DeletePostgresqlRequest>,
    ) -> Result<Response<DeletePostgresqlResponse>, Status> {
        Err(unimp("delete_postgresql"))
    }

    async fn get_postgresql(
        &self,
        _: Request<GetPostgresqlRequest>,
    ) -> Result<Response<GetPostgresqlResponse>, Status> {
        Err(unimp("get_postgresql"))
    }

    async fn list_postgresqls(
        &self,
        _: Request<ListPostgresqlsRequest>,
    ) -> Result<Response<ListPostgresqlsResponse>, Status> {
        Err(unimp("list_postgresqls"))
    }

    async fn create_security_group(
        &self,
        _: Request<CreateSecurityGroupRequest>,
    ) -> Result<Response<CreateSecurityGroupResponse>, Status> {
        Err(unimp("create_security_group"))
    }

    async fn get_security_group(
        &self,
        _: Request<GetSecurityGroupRequest>,
    ) -> Result<Response<GetSecurityGroupResponse>, Status> {
        Err(unimp("get_security_group"))
    }

    async fn list_security_groups(
        &self,
        _: Request<ListSecurityGroupsRequest>,
    ) -> Result<Response<ListSecurityGroupsResponse>, Status> {
        Err(unimp("list_security_groups"))
    }

    async fn delete_security_group(
        &self,
        _: Request<DeleteSecurityGroupRequest>,
    ) -> Result<Response<DeleteSecurityGroupResponse>, Status> {
        Err(unimp("delete_security_group"))
    }

    async fn attach_security_group(
        &self,
        _: Request<AttachSecurityGroupRequest>,
    ) -> Result<Response<AttachSecurityGroupResponse>, Status> {
        Err(unimp("attach_security_group"))
    }

    async fn detach_security_group(
        &self,
        _: Request<DetachSecurityGroupRequest>,
    ) -> Result<Response<DetachSecurityGroupResponse>, Status> {
        Err(unimp("detach_security_group"))
    }

    async fn list_nodes(
        &self,
        _: Request<ListNodesRequest>,
    ) -> Result<Response<ListNodesResponse>, Status> {
        Err(unimp("list_nodes"))
    }

    async fn get_node(
        &self,
        _: Request<GetNodeRequest>,
    ) -> Result<Response<GetNodeResponse>, Status> {
        Err(unimp("get_node"))
    }

    async fn create_ssh_key(
        &self,
        _: Request<CreateSshKeyRequest>,
    ) -> Result<Response<CreateSshKeyResponse>, Status> {
        Err(unimp("create_ssh_key"))
    }

    async fn delete_ssh_key(
        &self,
        _: Request<DeleteSshKeyRequest>,
    ) -> Result<Response<DeleteSshKeyResponse>, Status> {
        Err(unimp("delete_ssh_key"))
    }

    async fn list_ssh_keys(
        &self,
        _: Request<ListSshKeysRequest>,
    ) -> Result<Response<ListSshKeysResponse>, Status> {
        Err(unimp("list_ssh_keys"))
    }

    async fn get_ssh_key(
        &self,
        _: Request<GetSshKeyRequest>,
    ) -> Result<Response<GetSshKeyResponse>, Status> {
        Err(unimp("get_ssh_key"))
    }

    async fn drain_node(
        &self,
        _: Request<DrainNodeRequest>,
    ) -> Result<Response<DrainNodeResponse>, Status> {
        Err(unimp("drain_node"))
    }

    async fn migrate_vm(
        &self,
        _: Request<MigrateVmRequest>,
    ) -> Result<Response<MigrateVmResponse>, Status> {
        Err(unimp("migrate_vm"))
    }

    async fn get_live_migrate_receive_status(
        &self,
        _: Request<GetLiveMigrateReceiveStatusRequest>,
    ) -> Result<Response<GetLiveMigrateReceiveStatusResponse>, Status> {
        Err(unimp("get_live_migrate_receive_status"))
    }

    async fn reset_live_migrate_receive(
        &self,
        _: Request<ResetLiveMigrateReceiveRequest>,
    ) -> Result<Response<ResetLiveMigrateReceiveResponse>, Status> {
        Err(unimp("reset_live_migrate_receive"))
    }

    async fn approve_node(
        &self,
        _: Request<ApproveNodeRequest>,
    ) -> Result<Response<ApproveNodeResponse>, Status> {
        Err(unimp("approve_node"))
    }

    async fn reject_node(
        &self,
        _: Request<RejectNodeRequest>,
    ) -> Result<Response<RejectNodeResponse>, Status> {
        Err(unimp("reject_node"))
    }

    async fn cordon_node(
        &self,
        _: Request<CordonNodeRequest>,
    ) -> Result<Response<CordonNodeResponse>, Status> {
        Err(unimp("cordon_node"))
    }

    async fn uncordon_node(
        &self,
        _: Request<UncordonNodeRequest>,
    ) -> Result<Response<UncordonNodeResponse>, Status> {
        Err(unimp("uncordon_node"))
    }

    async fn delete_node(
        &self,
        _: Request<DeleteNodeRequest>,
    ) -> Result<Response<DeleteNodeResponse>, Status> {
        Err(unimp("delete_node"))
    }

    async fn get_cluster_health(
        &self,
        _: Request<GetClusterHealthRequest>,
    ) -> Result<Response<GetClusterHealthResponse>, Status> {
        Err(unimp("get_cluster_health"))
    }

    async fn backup_cluster(
        &self,
        _: Request<BackupClusterRequest>,
    ) -> Result<Response<BackupClusterResponse>, Status> {
        Err(unimp("backup_cluster"))
    }

    async fn restore_cluster(
        &self,
        _: Request<RestoreClusterRequest>,
    ) -> Result<Response<RestoreClusterResponse>, Status> {
        Err(unimp("restore_cluster"))
    }

    async fn renew_node_cert(
        &self,
        _: Request<RenewNodeCertRequest>,
    ) -> Result<Response<RenewNodeCertResponse>, Status> {
        Err(unimp("renew_node_cert"))
    }

    async fn issue_node_bootstrap_cert(
        &self,
        _: Request<IssueNodeBootstrapCertRequest>,
    ) -> Result<Response<IssueNodeBootstrapCertResponse>, Status> {
        Err(unimp("issue_node_bootstrap_cert"))
    }

    async fn rotate_sub_ca(
        &self,
        _: Request<RotateSubCaRequest>,
    ) -> Result<Response<RotateSubCaResponse>, Status> {
        Err(unimp("rotate_sub_ca"))
    }

    async fn sign_node_csr(
        &self,
        _: Request<SignNodeCsrRequest>,
    ) -> Result<Response<SignNodeCsrResponse>, Status> {
        Err(unimp("sign_node_csr"))
    }

    async fn rotate_node_certs(
        &self,
        _: Request<RotateNodeCertsRequest>,
    ) -> Result<Response<RotateNodeCertsResponse>, Status> {
        Err(unimp("rotate_node_certs"))
    }

    async fn list_certificates(
        &self,
        _: Request<ListCertificatesRequest>,
    ) -> Result<Response<ListCertificatesResponse>, Status> {
        Err(unimp("list_certificates"))
    }

    async fn revoke_certificate(
        &self,
        _: Request<RevokeCertificateRequest>,
    ) -> Result<Response<RevokeCertificateResponse>, Status> {
        Err(unimp("revoke_certificate"))
    }

    async fn get_crl(&self, _: Request<GetCrlRequest>) -> Result<Response<GetCrlResponse>, Status> {
        Err(unimp("get_crl"))
    }

    async fn get_pki_status(
        &self,
        _: Request<GetPkiStatusRequest>,
    ) -> Result<Response<GetPkiStatusResponse>, Status> {
        Err(unimp("get_pki_status"))
    }

    async fn reload_tls(
        &self,
        _: Request<ReloadTlsRequest>,
    ) -> Result<Response<ReloadTlsResponse>, Status> {
        Err(unimp("reload_tls"))
    }

    async fn get_network_overview(
        &self,
        _: Request<GetNetworkOverviewRequest>,
    ) -> Result<Response<GetNetworkOverviewResponse>, Status> {
        Ok(Response::new(GetNetworkOverviewResponse {
            default_gateway_interface: "eno1".into(),
            default_external_ip: "203.0.113.10".into(),
            default_gateway_ip: "10.0.0.1".into(),
            default_internal_netmask: "255.255.255.0".into(),
            nodes: vec![NodeNetworkInfo {
                node_id: "node-mock-a".into(),
                hostname: "mock-host-alpha".into(),
                address: "10.0.0.10:9443".into(),
                gateway_interface: "eno1".into(),
                disable_vxlan: false,
                interfaces: vec![
                    NetworkInterfaceDetail {
                        name: "eno1".into(),
                        mac_address: "aa:bb:cc:dd:ee:01".into(),
                        state: "UP".into(),
                        mtu: 1500,
                        addresses: vec!["10.0.0.10/24".into()],
                    },
                    NetworkInterfaceDetail {
                        name: "br-default".into(),
                        mac_address: "aa:bb:cc:dd:ee:02".into(),
                        state: "UP".into(),
                        mtu: 1500,
                        addresses: vec!["10.100.0.1/24".into()],
                    },
                    NetworkInterfaceDetail {
                        name: "tap-vm1".into(),
                        mac_address: "aa:bb:cc:dd:ee:03".into(),
                        state: "UP".into(),
                        mtu: 1500,
                        addresses: vec![],
                    },
                    NetworkInterfaceDetail {
                        name: "vxlan100".into(),
                        mac_address: "aa:bb:cc:dd:ee:04".into(),
                        state: "UP".into(),
                        mtu: 1450,
                        addresses: vec!["10.50.0.1/24".into()],
                    },
                    NetworkInterfaceDetail {
                        name: "lo".into(),
                        mac_address: "00:00:00:00:00:00".into(),
                        state: "UP".into(),
                        mtu: 65536,
                        addresses: vec!["127.0.0.1/8".into()],
                    },
                ],
            }],
        }))
    }

    async fn get_storage_overview(
        &self,
        _: Request<GetStorageOverviewRequest>,
    ) -> Result<Response<GetStorageOverviewResponse>, Status> {
        Ok(Response::new(GetStorageOverviewResponse {
            approved_nodes: 1,
            nodes_disk_inventory_ok: 1,
            backend_filesystem_nodes: 0,
            backend_lvm_nodes: 1,
            backend_zfs_nodes: 0,
            backend_ceph_nodes: 0,
            backend_unspecified_nodes: 0,
            nodes_luks_tpm2: 0,
            nodes_luks_keyfile: 1,
            nodes_luks_unknown: 0,
            total_block_devices: 2,
            nodes: vec![NodeStorageOverview {
                node_id: "node-mock-a".into(),
                hostname: "mock-host-alpha".into(),
                address: "10.0.0.10:9443".into(),
                storage_backend: StorageBackendType::Lvm as i32,
                luks_method: "key-file".into(),
                disk_inventory_ok: true,
                disks: vec![
                    StorageDiskDetail {
                        name: "sda".into(),
                        path: "/dev/sda".into(),
                        size: "500G".into(),
                        model: "Mock SSD".into(),
                        fstype: String::new(),
                        mountpoint: String::new(),
                    },
                    StorageDiskDetail {
                        name: "nvme0n1".into(),
                        path: "/dev/nvme0n1".into(),
                        size: "2T".into(),
                        model: "Mock NVMe".into(),
                        fstype: "zfs".into(),
                        mountpoint: "/var/lib/kcore".into(),
                    },
                ],
                lvm_inventory_ok: false,
                lvm_volume_groups: vec![],
                lvm_logical_volumes: vec![],
                lvm_physical_volumes: vec![],
            }],
        }))
    }

    async fn get_compliance_report(
        &self,
        _: Request<GetComplianceReportRequest>,
    ) -> Result<Response<GetComplianceReportResponse>, Status> {
        Ok(Response::new(GetComplianceReportResponse {
            controller_version: "mock-controller-0.1-test".into(),
            crypto_library: "rustls (integration test)".into(),
            tls13_cipher_suites: vec!["TEST_SUITE_A".into()],
            tls12_cipher_suites: vec![],
            kx_groups: vec!["TEST_KX".into()],
            excluded_algorithms: vec![],
            mtls_enabled: true,
            access_control: vec![AccessControlEntry {
                rpc_method: "ListVms".into(),
                allowed_identities: "role:node".into(),
                required_operator_role: "read-only".into(),
            }],
            total_nodes: 1,
            approved_nodes: 1,
            pending_nodes: 0,
            rejected_nodes: 0,
            total_vms: 1,
            running_vms: 1,
            stopped_vms: 0,
            total_networks: 1,
            nat_networks: 1,
            bridge_networks: 0,
            vxlan_networks: 0,
            sub_ca_enabled: false,
            cert_auto_renewal_days: 30,
            nodes_expiring_30d: 0,
            nodes_cert_unknown: 0,
            nodes: vec![],
            nodes_luks_tpm2: 0,
            nodes_luks_keyfile: 0,
            nodes_luks_unknown: 0,
        }))
    }

    async fn list_audit_events(
        &self,
        _: Request<ListAuditEventsRequest>,
    ) -> Result<Response<ListAuditEventsResponse>, Status> {
        Ok(Response::new(ListAuditEventsResponse {
            events: vec![
                AuditEvent {
                    id: 2,
                    actor: "kctl".into(),
                    action: "CreateVm".into(),
                    resource: "vm/web-01".into(),
                    created_at: "2026-08-04T12:00:00.000Z".into(),
                    detail: String::new(),
                },
                AuditEvent {
                    id: 1,
                    actor: "kcore-node-a".into(),
                    action: "RegisterNode".into(),
                    resource: "node/node-mock-a".into(),
                    created_at: "2026-08-01T00:00:00.000Z".into(),
                    detail: "joined".into(),
                },
            ],
        }))
    }

    async fn get_crypto_config(
        &self,
        _: Request<GetCryptoConfigRequest>,
    ) -> Result<Response<GetCryptoConfigResponse>, Status> {
        Ok(Response::new(GetCryptoConfigResponse::default()))
    }

    async fn export_sbom(
        &self,
        _: Request<ExportSbomRequest>,
    ) -> Result<Response<ExportSbomResponse>, Status> {
        Ok(Response::new(ExportSbomResponse::default()))
    }

    async fn list_volumes(
        &self,
        _: Request<ListVolumesRequest>,
    ) -> Result<Response<ListVolumesResponse>, Status> {
        Ok(Response::new(ListVolumesResponse {
            volumes: vec![VolumeInfo {
                id: "vol-mock-1".into(),
                name: "mock-pgdata".into(),
                role: "data".into(),
                attach_state: "attached".into(),
                serial: "abc123".into(),
                pool: "kcore-vms".into(),
                image: "kcore-vol-abc123".into(),
                slot: 1,
                vm_id: "vm-mock-1".into(),
                vm_name: "mock-vm".into(),
                node_id: "node-mock-a".into(),
                storage_backend: "ceph".into(),
                storage_size_bytes: 10 * 1024 * 1024 * 1024,
                backend_handle: "/dev/rbd/kcore-vms/kcore-vol-abc123".into(),
                image_format: "raw".into(),
                vm_state: 0,
                guest_visible_bytes: -1,
                guest_checked_at: String::new(),
                encrypted: false,
            }],
        }))
    }

    async fn create_volume(
        &self,
        _: Request<CreateVolumeRequest>,
    ) -> Result<Response<CreateVolumeResponse>, Status> {
        Err(unimp("create_volume"))
    }

    async fn get_volume(
        &self,
        _: Request<GetVolumeRequest>,
    ) -> Result<Response<GetVolumeResponse>, Status> {
        Err(unimp("get_volume"))
    }

    async fn attach_volume(
        &self,
        _: Request<AttachVolumeRequest>,
    ) -> Result<Response<AttachVolumeResponse>, Status> {
        Err(unimp("attach_volume"))
    }

    async fn detach_volume(
        &self,
        _: Request<DetachVolumeRequest>,
    ) -> Result<Response<DetachVolumeResponse>, Status> {
        Err(unimp("detach_volume"))
    }

    async fn delete_volume(
        &self,
        _: Request<DeleteVolumeRequest>,
    ) -> Result<Response<DeleteVolumeResponse>, Status> {
        Err(unimp("delete_volume"))
    }

    async fn list_vm_operations(
        &self,
        _: Request<ListVmOperationsRequest>,
    ) -> Result<Response<ListVmOperationsResponse>, Status> {
        Ok(Response::new(ListVmOperationsResponse {
            operations: vec![VmOperation {
                id: "op-mock-1".into(),
                vm_id: "vm-mock-1".into(),
                kind: "live_migrate".into(),
                phase: "Preparing".into(),
                source_node: "node-mock-a".into(),
                target_node: "node-mock-b".into(),
                cancel_requested: false,
                send_succeeded: false,
                detail_json: "{}".into(),
                started_at: None,
                updated_at: None,
                finished_at: None,
            }],
        }))
    }

    async fn get_vm_operation(
        &self,
        _: Request<GetVmOperationRequest>,
    ) -> Result<Response<GetVmOperationResponse>, Status> {
        Err(unimp("get_vm_operation"))
    }

    async fn cancel_vm_operation(
        &self,
        _: Request<CancelVmOperationRequest>,
    ) -> Result<Response<CancelVmOperationResponse>, Status> {
        Err(unimp("cancel_vm_operation"))
    }

    async fn create_volume_snapshot(
        &self,
        _: Request<CreateVolumeSnapshotRequest>,
    ) -> Result<Response<CreateVolumeSnapshotResponse>, Status> {
        Err(unimp("create_volume_snapshot"))
    }

    async fn list_volume_snapshots(
        &self,
        _: Request<ListVolumeSnapshotsRequest>,
    ) -> Result<Response<ListVolumeSnapshotsResponse>, Status> {
        Err(unimp("list_volume_snapshots"))
    }

    async fn delete_volume_snapshot(
        &self,
        _: Request<DeleteVolumeSnapshotRequest>,
    ) -> Result<Response<DeleteVolumeSnapshotResponse>, Status> {
        Err(unimp("delete_volume_snapshot"))
    }

    async fn restore_volume(
        &self,
        _: Request<RestoreVolumeRequest>,
    ) -> Result<Response<RestoreVolumeResponse>, Status> {
        Err(unimp("restore_volume"))
    }

    async fn flatten_volume(
        &self,
        _: Request<FlattenVolumeRequest>,
    ) -> Result<Response<FlattenVolumeResponse>, Status> {
        Err(unimp("flatten_volume"))
    }

    async fn resize_volume(
        &self,
        _: Request<ResizeVolumeRequest>,
    ) -> Result<Response<ResizeVolumeResponse>, Status> {
        Err(unimp("resize_volume"))
    }

    async fn encrypt_volume(
        &self,
        _: Request<EncryptVolumeRequest>,
    ) -> Result<Response<EncryptVolumeResponse>, Status> {
        Err(unimp("encrypt_volume"))
    }

    async fn create_snapshot_policy(
        &self,
        _: Request<CreateSnapshotPolicyRequest>,
    ) -> Result<Response<CreateSnapshotPolicyResponse>, Status> {
        Err(unimp("create_snapshot_policy"))
    }

    async fn get_snapshot_policy(
        &self,
        _: Request<GetSnapshotPolicyRequest>,
    ) -> Result<Response<GetSnapshotPolicyResponse>, Status> {
        Err(unimp("get_snapshot_policy"))
    }

    async fn list_snapshot_policies(
        &self,
        _: Request<ListSnapshotPoliciesRequest>,
    ) -> Result<Response<ListSnapshotPoliciesResponse>, Status> {
        Err(unimp("list_snapshot_policies"))
    }

    async fn delete_snapshot_policy(
        &self,
        _: Request<DeleteSnapshotPolicyRequest>,
    ) -> Result<Response<DeleteSnapshotPolicyResponse>, Status> {
        Err(unimp("delete_snapshot_policy"))
    }

    async fn create_disk_layout(
        &self,
        _: Request<CreateDiskLayoutRequest>,
    ) -> Result<Response<CreateDiskLayoutResponse>, Status> {
        Err(unimp("create_disk_layout"))
    }

    async fn get_disk_layout(
        &self,
        _: Request<GetDiskLayoutRequest>,
    ) -> Result<Response<GetDiskLayoutResponse>, Status> {
        Err(unimp("get_disk_layout"))
    }

    async fn list_disk_layouts(
        &self,
        _: Request<ListDiskLayoutsRequest>,
    ) -> Result<Response<ListDiskLayoutsResponse>, Status> {
        Err(unimp("list_disk_layouts"))
    }

    async fn delete_disk_layout(
        &self,
        _: Request<DeleteDiskLayoutRequest>,
    ) -> Result<Response<DeleteDiskLayoutResponse>, Status> {
        Err(unimp("delete_disk_layout"))
    }

    async fn classify_disk_layout(
        &self,
        _: Request<ClassifyDiskLayoutRequest>,
    ) -> Result<Response<ClassifyDiskLayoutResponse>, Status> {
        Err(unimp("classify_disk_layout"))
    }

    async fn create_ceph_cluster(
        &self,
        _: Request<CreateCephClusterRequest>,
    ) -> Result<Response<CreateCephClusterResponse>, Status> {
        Err(unimp("create_ceph_cluster"))
    }

    async fn get_ceph_cluster(
        &self,
        _: Request<GetCephClusterRequest>,
    ) -> Result<Response<GetCephClusterResponse>, Status> {
        Err(unimp("get_ceph_cluster"))
    }

    async fn list_ceph_clusters(
        &self,
        _: Request<ListCephClustersRequest>,
    ) -> Result<Response<ListCephClustersResponse>, Status> {
        Ok(Response::new(ListCephClustersResponse {
            ceph_clusters: vec![CephCluster {
                name: "mock-ceph".into(),
                generation: 2,
                spec: Some(CephClusterSpec {
                    fsid: "11111111-2222-3333-4444-555555555555".into(),
                    public_network: "10.20.0.0/24".into(),
                    cluster_network: "10.21.0.0/24".into(),
                    size: 3,
                    min_size: 2,
                    encrypt_osds: true,
                    nodes: vec![CephClusterNodeSpec {
                        node_id: "node-mock-a".into(),
                        mon_addr: "10.20.0.10:6789".into(),
                        osd_device: "/dev/sdb".into(),
                        ..Default::default()
                    }],
                    ..Default::default()
                }),
                status: Some(CephClusterStatus {
                    phase: CephClusterPhase::Healthy as i32,
                    health_message: "HEALTH_OK".into(),
                    ..Default::default()
                }),
                ..Default::default()
            }],
        }))
    }

    async fn delete_ceph_cluster(
        &self,
        _: Request<DeleteCephClusterRequest>,
    ) -> Result<Response<DeleteCephClusterResponse>, Status> {
        Err(unimp("delete_ceph_cluster"))
    }

    async fn create_shared_filesystem(
        &self,
        _: Request<CreateSharedFilesystemRequest>,
    ) -> Result<Response<CreateSharedFilesystemResponse>, Status> {
        Err(unimp("create_shared_filesystem"))
    }

    async fn get_shared_filesystem(
        &self,
        _: Request<GetSharedFilesystemRequest>,
    ) -> Result<Response<GetSharedFilesystemResponse>, Status> {
        Err(unimp("get_shared_filesystem"))
    }

    async fn list_shared_filesystems(
        &self,
        _: Request<ListSharedFilesystemsRequest>,
    ) -> Result<Response<ListSharedFilesystemsResponse>, Status> {
        Ok(Response::new(ListSharedFilesystemsResponse {
            shared_filesystems: vec![SharedFilesystem {
                name: "mock-home".into(),
                spec: Some(SharedFilesystemSpec {
                    ceph_cluster: "mock-ceph".into(),
                    quota_bytes: 100 * 1024 * 1024 * 1024,
                    clients: vec![SharedFilesystemClientSpec {
                        name: "desk".into(),
                        paths: vec!["/".into()],
                    }],
                    ..Default::default()
                }),
                status: Some(SharedFilesystemStatus {
                    phase: SharedFilesystemPhase::Healthy as i32,
                    health_message: "HEALTH_OK".into(),
                    ..Default::default()
                }),
                ..Default::default()
            }],
        }))
    }

    async fn delete_shared_filesystem(
        &self,
        _: Request<DeleteSharedFilesystemRequest>,
    ) -> Result<Response<DeleteSharedFilesystemResponse>, Status> {
        Err(unimp("delete_shared_filesystem"))
    }

    async fn create_object_store(
        &self,
        _: Request<CreateObjectStoreRequest>,
    ) -> Result<Response<CreateObjectStoreResponse>, Status> {
        Err(unimp("create_object_store"))
    }

    async fn get_object_store(
        &self,
        _: Request<GetObjectStoreRequest>,
    ) -> Result<Response<GetObjectStoreResponse>, Status> {
        Err(unimp("get_object_store"))
    }

    async fn list_object_stores(
        &self,
        _: Request<ListObjectStoresRequest>,
    ) -> Result<Response<ListObjectStoresResponse>, Status> {
        Ok(Response::new(ListObjectStoresResponse {
            object_stores: vec![ObjectStore {
                name: "mock-s3".into(),
                spec: Some(ObjectStoreSpec {
                    ceph_cluster: "mock-ceph".into(),
                    members: vec!["node-mock-a".into()],
                    port: 7480,
                    tls: true,
                    ..Default::default()
                }),
                status: Some(ObjectStoreStatus {
                    phase: ObjectStorePhase::Healthy as i32,
                    health_message: "HEALTH_OK".into(),
                    ..Default::default()
                }),
                ..Default::default()
            }],
        }))
    }

    async fn delete_object_store(
        &self,
        _: Request<DeleteObjectStoreRequest>,
    ) -> Result<Response<DeleteObjectStoreResponse>, Status> {
        Err(unimp("delete_object_store"))
    }

    async fn create_object_user(
        &self,
        _: Request<CreateObjectUserRequest>,
    ) -> Result<Response<CreateObjectUserResponse>, Status> {
        Err(unimp("create_object_user"))
    }

    async fn delete_object_user(
        &self,
        _: Request<DeleteObjectUserRequest>,
    ) -> Result<Response<DeleteObjectUserResponse>, Status> {
        Err(unimp("delete_object_user"))
    }

    async fn create_cluster_update(
        &self,
        _: Request<CreateClusterUpdateRequest>,
    ) -> Result<Response<CreateClusterUpdateResponse>, Status> {
        Err(unimp("create_cluster_update"))
    }

    async fn get_cluster_update(
        &self,
        _: Request<GetClusterUpdateRequest>,
    ) -> Result<Response<GetClusterUpdateResponse>, Status> {
        Err(unimp("get_cluster_update"))
    }

    async fn list_cluster_updates(
        &self,
        _: Request<ListClusterUpdatesRequest>,
    ) -> Result<Response<ListClusterUpdatesResponse>, Status> {
        Err(unimp("list_cluster_updates"))
    }

    async fn plan_cluster_update(
        &self,
        _: Request<PlanClusterUpdateRequest>,
    ) -> Result<Response<PlanClusterUpdateResponse>, Status> {
        Err(unimp("plan_cluster_update"))
    }

    async fn approve_cluster_update(
        &self,
        _: Request<ApproveClusterUpdateRequest>,
    ) -> Result<Response<ApproveClusterUpdateResponse>, Status> {
        Err(unimp("approve_cluster_update"))
    }

    async fn cancel_cluster_update(
        &self,
        _: Request<CancelClusterUpdateRequest>,
    ) -> Result<Response<CancelClusterUpdateResponse>, Status> {
        Err(unimp("cancel_cluster_update"))
    }

    async fn rollback_cluster_update(
        &self,
        _: Request<RollbackClusterUpdateRequest>,
    ) -> Result<Response<RollbackClusterUpdateResponse>, Status> {
        Err(unimp("rollback_cluster_update"))
    }

    async fn create_operator(
        &self,
        _: Request<CreateOperatorRequest>,
    ) -> Result<Response<CreateOperatorResponse>, Status> {
        Err(unimp("create_operator"))
    }

    async fn delete_operator(
        &self,
        _: Request<DeleteOperatorRequest>,
    ) -> Result<Response<DeleteOperatorResponse>, Status> {
        Err(unimp("delete_operator"))
    }

    async fn list_operators(
        &self,
        _: Request<ListOperatorsRequest>,
    ) -> Result<Response<ListOperatorsResponse>, Status> {
        Err(unimp("list_operators"))
    }

    async fn get_operator(
        &self,
        _: Request<GetOperatorRequest>,
    ) -> Result<Response<GetOperatorResponse>, Status> {
        Err(unimp("get_operator"))
    }

    async fn grant_operator_role(
        &self,
        _: Request<GrantOperatorRoleRequest>,
    ) -> Result<Response<GrantOperatorRoleResponse>, Status> {
        Err(unimp("grant_operator_role"))
    }

    async fn revoke_operator_role(
        &self,
        _: Request<RevokeOperatorRoleRequest>,
    ) -> Result<Response<RevokeOperatorRoleResponse>, Status> {
        Err(unimp("revoke_operator_role"))
    }

    async fn issue_operator_cert(
        &self,
        _: Request<IssueOperatorCertRequest>,
    ) -> Result<Response<IssueOperatorCertResponse>, Status> {
        Err(unimp("issue_operator_cert"))
    }
}

#[tonic::async_trait]
impl ControllerAdmin for MockController {
    async fn apply_nix_config(
        &self,
        _: Request<ApplyNixConfigRequest>,
    ) -> Result<Response<ApplyNixConfigResponse>, Status> {
        Err(unimp("apply_nix_config"))
    }

    async fn get_replication_events(
        &self,
        _: Request<GetReplicationEventsRequest>,
    ) -> Result<Response<GetReplicationEventsResponse>, Status> {
        Err(unimp("get_replication_events"))
    }

    async fn ack_replication_events(
        &self,
        _: Request<AckReplicationEventsRequest>,
    ) -> Result<Response<AckReplicationEventsResponse>, Status> {
        Err(unimp("ack_replication_events"))
    }

    async fn get_replication_status(
        &self,
        _: Request<GetReplicationStatusRequest>,
    ) -> Result<Response<GetReplicationStatusResponse>, Status> {
        Ok(Response::new(GetReplicationStatusResponse {
            outbox_head_event_id: 42,
            outbox_size: 100,
            outgoing: vec![ReplicationOutgoingStatus {
                peer_id: "dc-west".into(),
                last_acked_event_id: 40,
                lag_events: 2,
            }],
            incoming: vec![ReplicationIncomingStatus {
                peer_endpoint: "10.0.1.1:9090".into(),
                last_pulled_event_id: 38,
                last_applied_event_id: 37,
            }],
            unresolved_conflicts: 0,
            pending_compensation_jobs: 0,
            failed_compensation_jobs: 0,
            materialization_backlog: 0,
            oldest_unresolved_conflict_age_seconds: 0,
            failed_reservations: 0,
            zero_manual_slo_healthy: true,
            zero_manual_slo_violations: vec![],
            failed_retryable_reservations: 0,
            failed_non_retryable_reservations: 0,
            retry_exhausted_reservations: 0,
        }))
    }

    async fn list_replication_conflicts(
        &self,
        _: Request<ListReplicationConflictsRequest>,
    ) -> Result<Response<ListReplicationConflictsResponse>, Status> {
        Ok(Response::new(ListReplicationConflictsResponse {
            conflicts: vec![],
        }))
    }

    async fn resolve_replication_conflict(
        &self,
        _: Request<ResolveReplicationConflictRequest>,
    ) -> Result<Response<ResolveReplicationConflictResponse>, Status> {
        Err(unimp("resolve_replication_conflict"))
    }
}

/// Binds an ephemeral port and serves the mock controller until the process exits.
pub async fn spawn_mock_controller() -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("mock controller bind");
    let addr = listener.local_addr().expect("local_addr");
    let incoming = TcpListenerStream::new(listener);
    tokio::spawn(async move {
        Server::builder()
            .add_service(ControllerServer::new(MockController))
            .add_service(ControllerAdminServer::new(MockController))
            .serve_with_incoming(incoming)
            .await
            .expect("mock controller serve");
    });
    tokio::time::sleep(std::time::Duration::from_millis(80)).await;
    addr
}
