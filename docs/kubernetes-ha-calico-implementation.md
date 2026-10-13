# Kubernetes HA with Calico Implementation Plan

**Status:** Design phase  
**Target:** Production-ready Kubernetes on kcoreOS with 3 control planes + 3 workers  
**CNI:** Calico (not Flannel)  
**Working tree:** `/mnt/data/dev/kcore/kcore-rust-k8s`

## Executive Summary

This plan implements a production-grade Kubernetes cluster directly on kcoreOS nodes using:
- **3 control plane nodes** with stacked etcd (etcd runs on the same nodes as kube-apiserver)
- **3 worker nodes** for workload execution
- **Calico CNI** for pod networking (BGP-capable, production-grade network policy support)
- **NixOS services.kubernetes** module as the foundation
- **Full Rust/gRPC automation** - zero manual steps after initial declaration

All nodes run on bare metal kcoreOS. Guest VMs continue using cloud-hypervisor. This is the host-service pattern used by Ceph.

## Architecture

### Node Roles

```
Control Plane (3 nodes):
├─ node-105 (192.168.40.105) - First control plane, bootstrap etcd
├─ node-107 (192.168.40.107) - Additional control plane
└─ node-119 (192.168.40.119) - Additional control plane

Workers (3 nodes):
├─ node-103 (192.168.40.103) - Worker
├─ worker-2 (192.168.40.104) - Worker
└─ worker-3 (192.168.40.106) - Worker

Virtual IP (HAProxy/keepalived): 192.168.40.200
```

### Network Layout

```
Management Network: 192.168.40.0/24 (kcore control plane, Kubernetes API)
Ceph Network:       192.168.47.0/24 (existing, untouched)
Pod CIDR:           10.244.0.0/16   (Calico overlay)
Service CIDR:       10.96.0.0/12    (ClusterIP range)
Calico BGP:         Disabled initially (later: peer with datacenter routers)
```

**Calico configuration:**
- VXLAN mode initially (compatible with existing kcore VXLAN on different bridges)
- No IP-in-IP (avoids double encapsulation)
- NetworkPolicy enforcement enabled from day 1
- Option to enable BGP peering in phase 7

## Example Manifest

```yaml
kind: KubernetesCluster
metadata:
  name: lab-ha
  generation: 1
spec:
  clusterCidr: 10.244.0.0/16
  serviceCidr: 10.96.0.0/12
  clusterDns: 10.96.0.10
  apiServerEndpoint: 192.168.40.200:6443  # VIP managed by kcore
  cni: calico                              # Not flannel
  calicoConfig:
    mode: vxlan                            # or "bgp" later
    mtu: 1450                              # Leave room for VXLAN overhead
    ipipMode: Never                        # No IP-in-IP
    vxlanMode: Always
    natOutgoing: true
  controlPlane:
    nodes:
      - nodeId: node-105
        advertiseAddress: 192.168.40.105
        iface: enp6s0
        initialCluster: true               # Bootstrap etcd
      - nodeId: node-107
        advertiseAddress: 192.168.40.107
        iface: enp6s0
        joinExisting: true
      - nodeId: node-119
        advertiseAddress: 192.168.40.119
        iface: enp6s0
        joinExisting: true
  workers:
    nodes:
      - nodeId: node-103
        advertiseAddress: 192.168.40.103
        iface: enp6s0
      - nodeId: worker-2
        advertiseAddress: 192.168.40.104
        iface: enp6s0
      - nodeId: worker-3
        advertiseAddress: 192.168.40.106
        iface: enp6s0
  highAvailability:
    enabled: true
    vipAddress: 192.168.40.200
    vipInterface: enp6s0
    method: keepalived                     # kcore manages keepalived on control planes
```

## gRPC API Definition

Add to `proto/controller.proto`:

```protobuf
// ============================================================================
// Kubernetes Cluster Management
// ============================================================================

message CalicoConfig {
  string mode = 1;                  // "vxlan", "bgp", "vxlan-bgp"
  int32 mtu = 2;                    // Default 1450
  string ipip_mode = 3;             // "Never", "Always", "CrossSubnet"
  string vxlan_mode = 4;            // "Never", "Always", "CrossSubnet"
  bool nat_outgoing = 5;            // SNAT for pod-to-external
  repeated string bgp_peers = 6;    // ASN:IP pairs for BGP mode
}

message KubernetesNodeSpec {
  string node_id = 1;
  string role = 2;                  // "control-plane" or "worker"
  string advertise_address = 3;     // Static IP for this node
  string iface = 4;                 // Physical NIC (e.g., enp6s0)
  bool initial_cluster = 5;         // true for the first control plane (etcd bootstrap)
  bool join_existing = 6;           // true for additional control planes
}

message HighAvailabilityConfig {
  bool enabled = 1;
  string vip_address = 2;           // Virtual IP for kube-apiserver
  string vip_interface = 3;         // NIC to bind VIP
  string method = 4;                // "keepalived" (kcore-managed)
}

message KubernetesClusterSpec {
  string cluster_cidr = 1;
  string service_cidr = 2;
  string cluster_dns = 3;
  string api_server_endpoint = 4;   // VIP:port
  string cni = 5;                   // "calico" (not "flannel")
  CalicoConfig calico_config = 6;
  repeated KubernetesNodeSpec control_plane_nodes = 7;
  repeated KubernetesNodeSpec worker_nodes = 8;
  HighAvailabilityConfig high_availability = 9;
}

message KubernetesClusterStatus {
  string phase = 1;                 // pending, bootstrapping, scaling, ready, degraded, failed
  string message = 2;
  int32 ready_control_planes = 3;
  int32 ready_workers = 4;
  google.protobuf.Timestamp last_updated = 5;
  repeated string active_node_ids = 6;
}

message KubernetesCluster {
  string name = 1;
  int64 generation = 2;
  KubernetesClusterSpec spec = 3;
  KubernetesClusterStatus status = 4;
}

// RPCs

rpc PlanKubernetesCluster(PlanKubernetesClusterRequest) returns (PlanKubernetesClusterResponse);
rpc CreateKubernetesCluster(CreateKubernetesClusterRequest) returns (CreateKubernetesClusterResponse);
rpc UpdateKubernetesCluster(UpdateKubernetesClusterRequest) returns (UpdateKubernetesClusterResponse);
rpc GetKubernetesCluster(GetKubernetesClusterRequest) returns (GetKubernetesClusterResponse);
rpc ListKubernetesClusters(ListKubernetesClustersRequest) returns (ListKubernetesClustersResponse);
rpc DeleteKubernetesCluster(DeleteKubernetesClusterRequest) returns (DeleteKubernetesClusterResponse);
rpc ScaleKubernetesCluster(ScaleKubernetesClusterRequest) returns (ScaleKubernetesClusterResponse);
rpc GetKubernetesClusterHealth(GetKubernetesClusterHealthRequest) returns (GetKubernetesClusterHealthResponse);

message PlanKubernetesClusterRequest {
  string name = 1;
  KubernetesClusterSpec spec = 2;
}

message PlanKubernetesClusterResponse {
  string action = 1;                // "create", "update", "replace", "unchanged"
  repeated string blocking_issues = 2;
  repeated string warnings = 3;
  repeated NodePlanResult node_plans = 4;
  string rendered_nix_preview = 5;  // Show operator what will be applied
}

message NodePlanResult {
  string node_id = 1;
  bool preflight_passed = 2;
  repeated string issues = 3;
  string nix_action = 4;            // "build", "rebuild", "no-change"
}

message CreateKubernetesClusterRequest {
  string name = 1;
  KubernetesClusterSpec spec = 2;
  bool confirm = 3;                 // Must be true after successful plan
}

message CreateKubernetesClusterResponse {
  KubernetesCluster cluster = 1;
}

message UpdateKubernetesClusterRequest {
  string name = 1;
  KubernetesClusterSpec spec = 2;
  bool confirm = 3;
}

message UpdateKubernetesClusterResponse {
  KubernetesCluster cluster = 1;
}

message GetKubernetesClusterRequest {
  string name = 1;
}

message GetKubernetesClusterResponse {
  KubernetesCluster cluster = 1;
}

message ListKubernetesClustersRequest {}

message ListKubernetesClustersResponse {
  repeated KubernetesCluster clusters = 1;
}

message DeleteKubernetesClusterRequest {
  string name = 1;
  bool confirm = 2;
  bool force = 3;                   // Delete even if nodes unreachable
}

message DeleteKubernetesClusterResponse {
  string message = 1;
}

message ScaleKubernetesClusterRequest {
  string name = 1;
  repeated KubernetesNodeSpec add_nodes = 2;
  repeated string remove_node_ids = 3;
  bool confirm = 4;
}

message ScaleKubernetesClusterResponse {
  KubernetesCluster cluster = 1;
}

message GetKubernetesClusterHealthRequest {
  string name = 1;
}

message GetKubernetesClusterHealthResponse {
  string overall_health = 1;        // "healthy", "degraded", "critical"
  int32 ready_control_planes = 2;
  int32 total_control_planes = 3;
  int32 ready_workers = 4;
  int32 total_workers = 5;
  bool etcd_healthy = 6;
  int32 etcd_member_count = 7;
  bool api_server_reachable = 8;
  repeated NodeHealth node_health = 9;
}

message NodeHealth {
  string node_id = 1;
  string kubernetes_status = 2;     // "Ready", "NotReady", "Unknown"
  bool kubelet_active = 3;
  bool etcd_active = 4;             // Only for control planes
  bool api_server_active = 5;       // Only for control planes
}
```

Add to `proto/node.proto`:

```protobuf
// Node agent RPC for Kubernetes config application

rpc ApplyKubernetesConfig(ApplyKubernetesConfigRequest) returns (ApplyKubernetesConfigResponse);
rpc GetKubernetesStatus(GetKubernetesStatusRequest) returns (GetKubernetesStatusResponse);
rpc DrainKubernetesNode(DrainKubernetesNodeRequest) returns (DrainKubernetesNodeResponse);

message ApplyKubernetesConfigRequest {
  string role = 1;                  // "control-plane" or "worker"
  string cluster_name = 2;
  string cluster_cidr = 3;
  string service_cidr = 4;
  string cluster_dns = 5;
  string advertise_address = 6;
  string iface = 7;
  string api_server_endpoint = 8;   // VIP:port for workers and additional control planes
  bool initial_cluster = 9;         // true only for first control plane
  string cni = 10;                  // "calico"
  CalicoConfig calico_config = 11;
  string etcd_initial_cluster = 12; // "node1=https://ip1:2380,node2=https://ip2:2380,..."
  bool ha_enabled = 13;
  string vip_address = 14;          // For keepalived
  string vip_interface = 15;
  bytes kubeconfig_data = 16;       // For workers: admin kubeconfig from control plane
}

message ApplyKubernetesConfigResponse {
  string apply_id = 1;
  string status = 2;                // "started", "completed", "failed"
  string message = 3;
}

message GetKubernetesStatusRequest {}

message GetKubernetesStatusResponse {
  bool enabled = 1;
  string role = 2;
  bool kubelet_active = 3;
  bool api_server_active = 4;
  bool etcd_active = 5;
  bool calico_node_active = 6;
  string node_status = 7;           // From kubectl get nodes
  int32 running_pods = 8;
}

message DrainKubernetesNodeRequest {
  bool delete_emptydir_data = 1;
  bool ignore_daemonsets = 2;
  int32 timeout_seconds = 3;
}

message DrainKubernetesNodeResponse {
  string status = 1;
  string message = 2;
}
```

## NixOS Module: `modules/kcore-kubernetes.nix`

```nix
{ config, lib, pkgs, ... }:

let
  cfg = config.kcore.kubernetes;
  
  # Calico version compatible with NixOS kubernetes module
  calicoVersion = "v3.27.0";
  
  calicoManifests = pkgs.writeText "calico.yaml" ''
    # Calico VXLAN configuration
    # Generated by kcore-kubernetes module
    apiVersion: operator.tigera.io/v1
    kind: Installation
    metadata:
      name: default
    spec:
      calicoNetwork:
        bgp: ${if cfg.calicoConfig.mode == "bgp" || cfg.calicoConfig.mode == "vxlan-bgp" then "Enabled" else "Disabled"}
        ipPools:
        - cidr: ${cfg.clusterCidr}
          encapsulation: ${if cfg.calicoConfig.vxlanMode == "Always" then "VXLAN" else "None"}
          natOutgoing: ${if cfg.calicoConfig.natOutgoing then "Enabled" else "Disabled"}
          nodeSelector: all()
        mtu: ${toString cfg.calicoConfig.mtu}
      nodeAddressAutodetectionV4:
        interface: ${cfg.iface}
    ---
    apiVersion: operator.tigera.io/v1
    kind: APIServer
    metadata:
      name: default
    spec: {}
  '';

  isControlPlane = cfg.role == "control-plane";
  isWorker = cfg.role == "worker";

in
{
  options.kcore.kubernetes = {
    enable = lib.mkEnableOption "kcore Kubernetes services";
    
    role = lib.mkOption {
      type = lib.types.enum [ "control-plane" "worker" ];
      description = "Node role in the Kubernetes cluster";
    };
    
    clusterName = lib.mkOption {
      type = lib.types.str;
      default = "kcore";
    };
    
    clusterCidr = lib.mkOption {
      type = lib.types.str;
      default = "10.244.0.0/16";
      description = "Pod network CIDR";
    };
    
    serviceCidr = lib.mkOption {
      type = lib.types.str;
      default = "10.96.0.0/12";
      description = "Service network CIDR";
    };
    
    clusterDns = lib.mkOption {
      type = lib.types.str;
      default = "10.96.0.10";
    };
    
    advertiseAddress = lib.mkOption {
      type = lib.types.str;
      description = "IP address this node advertises to the cluster";
    };
    
    iface = lib.mkOption {
      type = lib.types.str;
      description = "Network interface for Kubernetes and Calico";
    };
    
    apiServerEndpoint = lib.mkOption {
      type = lib.types.str;
      description = "Kubernetes API server endpoint (VIP:port or first control plane)";
    };
    
    initialCluster = lib.mkOption {
      type = lib.types.bool;
      default = false;
      description = "Whether this is the bootstrap control plane node";
    };
    
    etcdInitialCluster = lib.mkOption {
      type = lib.types.str;
      default = "";
      description = "etcd initial-cluster string for HA";
    };
    
    cni = lib.mkOption {
      type = lib.types.str;
      default = "calico";
      description = "CNI plugin to use";
    };
    
    calicoConfig = {
      mode = lib.mkOption {
        type = lib.types.str;
        default = "vxlan";
      };
      
      mtu = lib.mkOption {
        type = lib.types.int;
        default = 1450;
      };
      
      vxlanMode = lib.mkOption {
        type = lib.types.str;
        default = "Always";
      };
      
      natOutgoing = lib.mkOption {
        type = lib.types.bool;
        default = true;
      };
    };
    
    highAvailability = {
      enabled = lib.mkOption {
        type = lib.types.bool;
        default = false;
      };
      
      vipAddress = lib.mkOption {
        type = lib.types.str;
        default = "";
      };
      
      vipInterface = lib.mkOption {
        type = lib.types.str;
        default = "";
      };
    };
  };
  
  config = lib.mkIf cfg.enable {
    # Assertions
    assertions = [
      {
        assertion = cfg.advertiseAddress != "";
        message = "kcore.kubernetes.advertiseAddress must be set";
      }
      {
        assertion = cfg.iface != "" && cfg.iface != "br0" && !(lib.hasPrefix "kbr-" cfg.iface);
        message = "kcore.kubernetes.iface must be set and cannot be br0 or kbr-*";
      }
      {
        assertion = !isWorker || cfg.apiServerEndpoint != "";
        message = "Worker nodes require apiServerEndpoint";
      }
      {
        assertion = !cfg.highAvailability.enabled || cfg.highAvailability.vipAddress != "";
        message = "HA enabled but vipAddress not set";
      }
    ];
    
    # Kubernetes service configuration
    services.kubernetes = {
      roles = if isControlPlane then [ "master" "node" ] else [ "node" ];
      
      masterAddress = cfg.apiServerEndpoint;
      
      easyCerts = cfg.initialCluster;  # Only bootstrap node generates certs
      
      apiserver = lib.mkIf isControlPlane {
        advertiseAddress = cfg.advertiseAddress;
        bindAddress = "0.0.0.0";
        securePort = 6443;
        serviceClusterIpRange = cfg.serviceCidr;
        extraOpts = ''
          --allow-privileged=true
          --enable-bootstrap-token-auth=true
        '';
      };
      
      controllerManager = lib.mkIf isControlPlane {
        extraOpts = ''
          --cluster-cidr=${cfg.clusterCidr}
          --service-cluster-ip-range=${cfg.serviceCidr}
          --allocate-node-cidrs=true
        '';
      };
      
      scheduler = lib.mkIf isControlPlane {
        extraOpts = "";
      };
      
      kubelet = {
        enable = true;
        address = "0.0.0.0";
        clusterDns = cfg.clusterDns;
        clusterDomain = "cluster.local";
        extraOpts = ''
          --network-plugin=cni
          --cni-bin-dir=/opt/cni/bin
          --cni-conf-dir=/etc/cni/net.d
        '';
      };
      
      proxy = {
        enable = true;
      };
      
      etcd = lib.mkIf isControlPlane {
        enable = true;
        listenClientUrls = [ "https://${cfg.advertiseAddress}:2379" ];
        advertiseClientUrls = [ "https://${cfg.advertiseAddress}:2379" ];
        listenPeerUrls = [ "https://${cfg.advertiseAddress}:2380" ];
        initialAdvertisePeerUrls = [ "https://${cfg.advertiseAddress}:2380" ];
        initialCluster = if cfg.etcdInitialCluster != "" then cfg.etcdInitialCluster else "${cfg.clusterName}=https://${cfg.advertiseAddress}:2380";
        initialClusterState = if cfg.initialCluster then "new" else "existing";
      };
    };
    
    # Keepalived for HA VIP (control plane only)
    services.keepalived = lib.mkIf (isControlPlane && cfg.highAvailability.enabled) {
      enable = true;
      vrrpInstances.kube_apiserver = {
        interface = cfg.highAvailability.vipInterface;
        state = if cfg.initialCluster then "MASTER" else "BACKUP";
        priority = if cfg.initialCluster then 100 else 50;
        virtualRouterId = 51;
        virtualIps = [
          { addr = cfg.highAvailability.vipAddress; }
        ];
      };
    };
    
    # Calico CNI installation
    systemd.services.calico-install = lib.mkIf (cfg.cni == "calico") {
      description = "Install Calico CNI";
      after = [ "kubernetes-apiserver.service" ];
      wants = [ "kubernetes-apiserver.service" ];
      wantedBy = [ "multi-user.target" ];
      
      serviceConfig = {
        Type = "oneshot";
        RemainAfterExit = true;
        ExecStart = pkgs.writeShellScript "install-calico" ''
          set -e
          
          # Wait for API server
          until ${pkgs.kubernetes}/bin/kubectl --kubeconfig=/var/lib/kubernetes/admin.conf get nodes 2>/dev/null; do
            echo "Waiting for kube-apiserver..."
            sleep 5
          done
          
          # Install Calico operator
          ${pkgs.kubernetes}/bin/kubectl --kubeconfig=/var/lib/kubernetes/admin.conf create -f https://raw.githubusercontent.com/projectcalico/calico/${calicoVersion}/manifests/tigera-operator.yaml || true
          
          # Wait for operator
          sleep 10
          
          # Apply Calico configuration
          ${pkgs.kubernetes}/bin/kubectl --kubeconfig=/var/lib/kubernetes/admin.conf apply -f ${calicoManifests}
          
          echo "Calico installation complete"
        '';
      };
    };
    
    # Firewall rules
    networking.firewall = {
      allowedTCPPorts = lib.optionals isControlPlane [
        6443   # kube-apiserver
        2379   # etcd client
        2380   # etcd peer
        10250  # kubelet
        10251  # kube-scheduler
        10252  # kube-controller-manager
      ] ++ lib.optionals isWorker [
        10250  # kubelet
      ];
      
      allowedUDPPorts = [
        4789   # Calico VXLAN
      ];
      
      allowedTCPPortRanges = [
        { from = 30000; to = 32767; }  # NodePort range
      ];
      
      # Calico BGP
      allowedTCPPorts = lib.optionals (cfg.calicoConfig.mode == "bgp" || cfg.calicoConfig.mode == "vxlan-bgp") [ 179 ];
    };
    
    # CNI binaries
    environment.systemPackages = with pkgs; [
      kubernetes
      cni-plugins
      etcd
    ];
    
    # CNI directories
    systemd.tmpfiles.rules = [
      "d /opt/cni 0755 root root -"
      "d /opt/cni/bin 0755 root root -"
      "d /etc/cni 0755 root root -"
      "d /etc/cni/net.d 0755 root root -"
      "L+ /opt/cni/bin - - - - ${pkgs.cni-plugins}/bin"
    ];
  };
}
```

## Rust Implementation

### 1. Controller: `crates/controller/src/kubernetes_cluster_reconciler.rs`

```rust
use crate::proto::{
    KubernetesCluster, KubernetesClusterSpec, KubernetesClusterStatus,
    PlanKubernetesClusterRequest, CreateKubernetesClusterRequest,
    ApplyKubernetesConfigRequest, NodePlanResult,
};
use crate::nix_codegen::generate_kubernetes_nix;
use crate::db::SqlitePool;
use crate::node_client::NodeAdminClient;
use anyhow::{Result, Context, bail};
use std::collections::HashMap;
use tracing::{info, warn, error};

pub struct KubernetesClusterReconciler {
    db: SqlitePool,
    node_clients: HashMap<String, NodeAdminClient>,
}

impl KubernetesClusterReconciler {
    pub fn new(db: SqlitePool) -> Self {
        Self {
            db,
            node_clients: HashMap::new(),
        }
    }
    
    /// Phase 1: Preflight checks on all nodes
    pub async fn plan(&mut self, req: PlanKubernetesClusterRequest) -> Result<PlanKubernetesClusterResponse> {
        let spec = req.spec.context("spec required")?;
        let mut blocking_issues = Vec::new();
        let mut warnings = Vec::new();
        let mut node_plans = Vec::new();
        
        // Validate CIDRs
        self.validate_cidrs(&spec, &mut blocking_issues)?;
        
        // Check all control plane nodes
        for cp_node in &spec.control_plane_nodes {
            let plan = self.plan_control_plane_node(&spec, cp_node).await?;
            if !plan.preflight_passed {
                blocking_issues.extend(plan.issues.clone());
            }
            node_plans.push(plan);
        }
        
        // Check all worker nodes
        for worker in &spec.worker_nodes {
            let plan = self.plan_worker_node(&spec, worker).await?;
            if !plan.preflight_passed {
                blocking_issues.extend(plan.issues.clone());
            }
            node_plans.push(plan);
        }
        
        // VIP reachability check
        if let Some(ha) = &spec.high_availability {
            if ha.enabled {
                if !self.check_vip_free(&ha.vip_address).await? {
                    blocking_issues.push(format!("VIP {} already in use", ha.vip_address));
                }
            }
        }
        
        // Render preview
        let first_cp = spec.control_plane_nodes.first()
            .context("at least one control plane required")?;
        let rendered_nix = generate_kubernetes_nix(&spec, first_cp, true)?;
        
        Ok(PlanKubernetesClusterResponse {
            action: if existing_cluster { "update" } else { "create" },
            blocking_issues,
            warnings,
            node_plans,
            rendered_nix_preview: Some(rendered_nix),
        })
    }
    
    async fn plan_control_plane_node(
        &mut self,
        spec: &KubernetesClusterSpec,
        node: &KubernetesNodeSpec,
    ) -> Result<NodePlanResult> {
        let mut issues = Vec::new();
        
        // Get node client
        let client = self.get_node_client(&node.node_id).await?;
        
        // Check interface exists
        if !client.check_interface_exists(&node.iface).await? {
            issues.push(format!("Interface {} not found", node.iface));
        }
        
        // Check interface is not DHCP
        if client.is_dhcp_managed(&node.iface).await? {
            issues.push(format!("Interface {} is DHCP-managed, requires static IP", node.iface));
        }
        
        // Check advertise address is on interface
        if !client.check_address_on_interface(&node.advertise_address, &node.iface).await? {
            issues.push(format!("Address {} not found on {}", node.advertise_address, node.iface));
        }
        
        // Check ports available (6443, 2379, 2380, 10250)
        let ports = vec![6443, 2379, 2380, 10250, 10251, 10252];
        for port in ports {
            if !client.check_port_free(&node.advertise_address, port).await? {
                issues.push(format!("Port {} already in use", port));
            }
        }
        
        // Check no CIDR overlap
        if self.check_cidr_overlap(&spec.cluster_cidr).await? {
            issues.push(format!("Pod CIDR {} overlaps with existing network", spec.cluster_cidr));
        }
        
        // Dry-build NixOS config
        let nix_config = generate_kubernetes_nix(spec, node, true)?;
        match client.dry_build_nix(&nix_config).await {
            Ok(_) => {},
            Err(e) => issues.push(format!("Nix dry-build failed: {}", e)),
        }
        
        Ok(NodePlanResult {
            node_id: node.node_id.clone(),
            preflight_passed: issues.is_empty(),
            issues,
            nix_action: "build".to_string(),
        })
    }
    
    async fn plan_worker_node(
        &mut self,
        spec: &KubernetesClusterSpec,
        node: &KubernetesNodeSpec,
    ) -> Result<NodePlanResult> {
        // Similar to control plane but skip etcd/apiserver port checks
        let mut issues = Vec::new();
        let client = self.get_node_client(&node.node_id).await?;
        
        // Interface checks
        if !client.check_interface_exists(&node.iface).await? {
            issues.push(format!("Interface {} not found", node.iface));
        }
        
        // Check port 10250 (kubelet) available
        if !client.check_port_free(&node.advertise_address, 10250).await? {
            issues.push("Port 10250 (kubelet) already in use".to_string());
        }
        
        Ok(NodePlanResult {
            node_id: node.node_id.clone(),
            preflight_passed: issues.is_empty(),
            issues,
            nix_action: "build".to_string(),
        })
    }
    
    /// Phase 2: Bootstrap first control plane
    pub async fn create(&mut self, req: CreateKubernetesClusterRequest) -> Result<KubernetesCluster> {
        if !req.confirm {
            bail!("confirm must be true after successful plan");
        }
        
        let spec = req.spec.context("spec required")?;
        
        // 1. Find bootstrap node (initial_cluster = true)
        let bootstrap_node = spec.control_plane_nodes.iter()
            .find(|n| n.initial_cluster)
            .context("no bootstrap control plane found")?;
        
        info!("Bootstrapping first control plane on {}", bootstrap_node.node_id);
        
        // 2. Generate PKI and initial config
        let bootstrap_config = self.generate_bootstrap_config(&spec, bootstrap_node)?;
        
        // 3. Apply to bootstrap node
        let client = self.get_node_client(&bootstrap_node.node_id).await?;
        client.apply_kubernetes_config(bootstrap_config).await?;
        
        // 4. Wait for API server ready
        self.wait_for_apiserver(&bootstrap_node.advertise_address, 6443).await?;
        
        // 5. Retrieve kubeconfig from bootstrap node
        let kubeconfig = client.get_kubeconfig().await?;
        
        // 6. Join additional control planes
        for cp_node in &spec.control_plane_nodes {
            if cp_node.initial_cluster {
                continue; // Skip bootstrap
            }
            
            info!("Joining control plane {}", cp_node.node_id);
            let join_config = self.generate_join_config(&spec, cp_node, &kubeconfig)?;
            let cp_client = self.get_node_client(&cp_node.node_id).await?;
            cp_client.apply_kubernetes_config(join_config).await?;
        }
        
        // 7. Wait for etcd quorum (3/3)
        self.wait_for_etcd_quorum(&kubeconfig, 3).await?;
        
        // 8. Join workers
        for worker in &spec.worker_nodes {
            info!("Joining worker {}", worker.node_id);
            let worker_config = self.generate_worker_config(&spec, worker, &kubeconfig)?;
            let worker_client = self.get_node_client(&worker.node_id).await?;
            worker_client.apply_kubernetes_config(worker_config).await?;
        }
        
        // 9. Install Calico (done by systemd service on bootstrap node)
        info!("Waiting for Calico to become ready");
        self.wait_for_calico(&kubeconfig).await?;
        
        // 10. Persist cluster state
        let cluster = self.persist_cluster(&req.name, &spec).await?;
        
        info!("Kubernetes cluster {} created successfully", req.name);
        Ok(cluster)
    }
    
    fn generate_bootstrap_config(
        &self,
        spec: &KubernetesClusterSpec,
        node: &KubernetesNodeSpec,
    ) -> Result<ApplyKubernetesConfigRequest> {
        let etcd_initial_cluster = self.build_etcd_initial_cluster(spec)?;
        
        Ok(ApplyKubernetesConfigRequest {
            role: "control-plane".to_string(),
            cluster_name: spec.cluster_name.clone(),
            cluster_cidr: spec.cluster_cidr.clone(),
            service_cidr: spec.service_cidr.clone(),
            cluster_dns: spec.cluster_dns.clone(),
            advertise_address: node.advertise_address.clone(),
            iface: node.iface.clone(),
            api_server_endpoint: spec.api_server_endpoint.clone(),
            initial_cluster: true,
            cni: spec.cni.clone(),
            calico_config: spec.calico_config.clone(),
            etcd_initial_cluster,
            ha_enabled: spec.high_availability.as_ref().map(|h| h.enabled).unwrap_or(false),
            vip_address: spec.high_availability.as_ref().map(|h| h.vip_address.clone()).unwrap_or_default(),
            vip_interface: spec.high_availability.as_ref().map(|h| h.vip_interface.clone()).unwrap_or_default(),
            kubeconfig_data: vec![],
        })
    }
    
    fn generate_join_config(
        &self,
        spec: &KubernetesClusterSpec,
        node: &KubernetesNodeSpec,
        kubeconfig: &[u8],
    ) -> Result<ApplyKubernetesConfigRequest> {
        let etcd_initial_cluster = self.build_etcd_initial_cluster(spec)?;
        
        Ok(ApplyKubernetesConfigRequest {
            role: "control-plane".to_string(),
            initial_cluster: false,
            kubeconfig_data: kubeconfig.to_vec(),
            etcd_initial_cluster,
            // ... other fields same as bootstrap
        })
    }
    
    fn generate_worker_config(
        &self,
        spec: &KubernetesClusterSpec,
        node: &KubernetesNodeSpec,
        kubeconfig: &[u8],
    ) -> Result<ApplyKubernetesConfigRequest> {
        Ok(ApplyKubernetesConfigRequest {
            role: "worker".to_string(),
            cluster_name: spec.cluster_name.clone(),
            advertise_address: node.advertise_address.clone(),
            iface: node.iface.clone(),
            api_server_endpoint: spec.api_server_endpoint.clone(),
            initial_cluster: false,
            kubeconfig_data: kubeconfig.to_vec(),
            cni: spec.cni.clone(),
            calico_config: spec.calico_config.clone(),
            // ... other fields
        })
    }
    
    fn build_etcd_initial_cluster(&self, spec: &KubernetesClusterSpec) -> Result<String> {
        let mut members = Vec::new();
        for (i, cp) in spec.control_plane_nodes.iter().enumerate() {
            members.push(format!(
                "{}=https://{}:2380",
                format!("control-plane-{}", i),
                cp.advertise_address
            ));
        }
        Ok(members.join(","))
    }
    
    async fn wait_for_apiserver(&self, address: &str, port: u16) -> Result<()> {
        // Poll https://address:port/healthz until 200 OK
        // Timeout: 5 minutes
        todo!("Implement API server health check")
    }
    
    async fn wait_for_etcd_quorum(&self, kubeconfig: &[u8], expected: usize) -> Result<()> {
        // Use etcdctl via kubectl exec on etcd pod
        // Check `etcdctl member list` returns expected members
        todo!("Implement etcd quorum check")
    }
    
    async fn wait_for_calico(&self, kubeconfig: &[u8]) -> Result<()> {
        // Poll `kubectl get pods -n calico-system` until all Running
        // Timeout: 10 minutes (pulls images)
        todo!("Implement Calico readiness check")
    }
    
    async fn persist_cluster(&self, name: &str, spec: &KubernetesClusterSpec) -> Result<KubernetesCluster> {
        // Insert into kubernetes_clusters table
        // Set status = "ready"
        todo!("Persist to SQLite")
    }
}
```

### 2. Node Agent: `crates/node-agent/src/grpc/admin.rs`

```rust
use crate::proto::{ApplyKubernetesConfigRequest, ApplyKubernetesConfigResponse};
use anyhow::{Result, Context};
use tokio::process::Command;
use std::path::Path;

impl NodeAdminService {
    pub async fn apply_kubernetes_config(
        &self,
        req: ApplyKubernetesConfigRequest,
    ) -> Result<ApplyKubernetesConfigResponse> {
        info!("Applying Kubernetes config for role: {}", req.role);
        
        // 1. Generate Nix fragment from request
        let nix_content = self.render_kubernetes_nix(&req)?;
        
        // 2. Write to /etc/nixos/kcore-kubernetes.nix
        tokio::fs::write("/etc/nixos/kcore-kubernetes.nix", &nix_content).await
            .context("Failed to write kcore-kubernetes.nix")?;
        
        // 3. If worker, write kubeconfig first
        if req.role == "worker" && !req.kubeconfig_data.is_empty() {
            tokio::fs::create_dir_all("/var/lib/kubernetes").await?;
            tokio::fs::write("/var/lib/kubernetes/admin.conf", &req.kubeconfig_data).await?;
        }
        
        // 4. Copy module from embedded binary
        let module_content = include_str!("../../../modules/kcore-kubernetes.nix");
        tokio::fs::write("/etc/nixos/modules/kcore-kubernetes.nix", module_content).await?;
        
        // 5. nixos-rebuild test (under apply lock)
        let _lock = self.apply_lock.lock().await;
        
        info!("Running nixos-rebuild test");
        let test_output = Command::new("nixos-rebuild")
            .arg("test")
            .output()
            .await
            .context("nixos-rebuild test failed")?;
        
        if !test_output.status.success() {
            let stderr = String::from_utf8_lossy(&test_output.stderr);
            anyhow::bail!("nixos-rebuild test failed: {}", stderr);
        }
        
        // 6. nixos-rebuild switch
        info!("Running nixos-rebuild switch");
        let switch_output = Command::new("nixos-rebuild")
            .arg("switch")
            .output()
            .await
            .context("nixos-rebuild switch failed")?;
        
        if !switch_output.status.success() {
            let stderr = String::from_utf8_lossy(&switch_output.stderr);
            anyhow::bail!("nixos-rebuild switch failed: {}", stderr);
        }
        
        // 7. Verify services started
        tokio::time::sleep(tokio::time::Duration::from_secs(10)).await;
        
        if req.role == "control-plane" {
            self.verify_service_active("kube-apiserver").await?;
            self.verify_service_active("etcd").await?;
        }
        self.verify_service_active("kubelet").await?;
        
        info!("Kubernetes config applied successfully");
        Ok(ApplyKubernetesConfigResponse {
            apply_id: uuid::Uuid::new_v4().to_string(),
            status: "completed".to_string(),
            message: "Kubernetes configured successfully".to_string(),
        })
    }
    
    fn render_kubernetes_nix(&self, req: &ApplyKubernetesConfigRequest) -> Result<String> {
        // Escape all strings through nix_escape
        let escaped_cluster_name = nix_escape(&req.cluster_name);
        let escaped_advertise = nix_escape(&req.advertise_address);
        // ... escape all fields
        
        let nix = format!(r#"
{{ ... }}: {{
  kcore.kubernetes = {{
    enable = true;
    role = "{}";
    clusterName = "{}";
    clusterCidr = "{}";
    serviceCidr = "{}";
    clusterDns = "{}";
    advertiseAddress = "{}";
    iface = "{}";
    apiServerEndpoint = "{}";
    initialCluster = {};
    cni = "{}";
    calicoConfig = {{
      mode = "{}";
      mtu = {};
      vxlanMode = "{}";
      natOutgoing = {};
    }};
    etcdInitialCluster = "{}";
    highAvailability = {{
      enabled = {};
      vipAddress = "{}";
      vipInterface = "{}";
    }};
  }};
}}
"#,
            req.role,
            escaped_cluster_name,
            nix_escape(&req.cluster_cidr),
            nix_escape(&req.service_cidr),
            nix_escape(&req.cluster_dns),
            escaped_advertise,
            nix_escape(&req.iface),
            nix_escape(&req.api_server_endpoint),
            req.initial_cluster,
            nix_escape(&req.cni),
            // Calico config
            nix_escape(&req.calico_config.mode),
            req.calico_config.mtu,
            nix_escape(&req.calico_config.vxlan_mode),
            req.calico_config.nat_outgoing,
            nix_escape(&req.etcd_initial_cluster),
            // HA
            req.ha_enabled,
            nix_escape(&req.vip_address),
            nix_escape(&req.vip_interface),
        );
        
        Ok(nix)
    }
    
    async fn verify_service_active(&self, service: &str) -> Result<()> {
        let output = Command::new("systemctl")
            .args(&["is-active", service])
            .output()
            .await?;
        
        if !output.status.success() {
            anyhow::bail!("Service {} is not active", service);
        }
        
        Ok(())
    }
}
```

### 3. CLI: `crates/kctl/src/commands/kubernetes.rs`

```rust
use clap::{Args, Subcommand};
use anyhow::Result;

#[derive(Args)]
pub struct KubernetesArgs {
    #[command(subcommand)]
    command: KubernetesCommand,
}

#[derive(Subcommand)]
enum KubernetesCommand {
    /// Plan Kubernetes cluster changes
    Plan {
        #[arg(short, long)]
        file: String,
    },
    /// Create or update Kubernetes cluster
    Apply {
        #[arg(short, long)]
        file: String,
    },
    /// Get Kubernetes cluster status
    Get {
        name: String,
    },
    /// List Kubernetes clusters
    List,
    /// Delete Kubernetes cluster
    Delete {
        name: String,
        #[arg(long)]
        confirm: bool,
    },
    /// Get cluster health
    Health {
        name: String,
    },
}

impl KubernetesArgs {
    pub async fn run(&self, ctx: &CliContext) -> Result<()> {
        match &self.command {
            KubernetesCommand::Plan { file } => {
                let spec = load_kubernetes_spec(file)?;
                let resp = ctx.controller_client.plan_kubernetes_cluster(spec).await?;
                
                if !resp.blocking_issues.is_empty() {
                    eprintln!("❌ Blocking issues:");
                    for issue in &resp.blocking_issues {
                        eprintln!("  - {}", issue);
                    }
                    std::process::exit(1);
                }
                
                println!("✅ Plan OK");
                println!("\nAction: {}", resp.action);
                println!("\nNode plans:");
                for node in &resp.node_plans {
                    println!("  {} - {}", node.node_id, if node.preflight_passed { "✅" } else { "❌" });
                }
                
                if let Some(preview) = resp.rendered_nix_preview {
                    println!("\nNix preview (first control plane):\n{}", preview);
                }
                
                Ok(())
            }
            
            KubernetesCommand::Apply { file } => {
                // 1. Load spec
                let spec = load_kubernetes_spec(file)?;
                
                // 2. Plan first
                println!("Planning...");
                let plan = ctx.controller_client.plan_kubernetes_cluster(spec.clone()).await?;
                
                if !plan.blocking_issues.is_empty() {
                    eprintln!("❌ Cannot apply: plan has blocking issues");
                    return Ok(());
                }
                
                // 3. Confirm
                println!("\n⚠️  This will rebuild NixOS on {} nodes", 
                    spec.control_plane_nodes.len() + spec.worker_nodes.len());
                print!("Continue? (yes/no): ");
                let mut input = String::new();
                std::io::stdin().read_line(&mut input)?;
                
                if input.trim() != "yes" {
                    println!("Aborted");
                    return Ok(());
                }
                
                // 4. Apply
                println!("Creating cluster...");
                let cluster = ctx.controller_client.create_kubernetes_cluster(
                    spec.metadata.name.clone(),
                    spec,
                    true, // confirm
                ).await?;
                
                println!("✅ Kubernetes cluster {} created", cluster.name);
                Ok(())
            }
            
            KubernetesCommand::Get { name } => {
                let cluster = ctx.controller_client.get_kubernetes_cluster(name).await?;
                println!("{}", serde_yaml::to_string(&cluster)?);
                Ok(())
            }
            
            KubernetesCommand::List => {
                let clusters = ctx.controller_client.list_kubernetes_clusters().await?;
                for cluster in clusters {
                    println!("{} - {} ({})", cluster.name, cluster.status.phase, cluster.generation);
                }
                Ok(())
            }
            
            KubernetesCommand::Delete { name, confirm } => {
                if !confirm {
                    eprintln!("❌ Must pass --confirm to delete cluster");
                    return Ok(());
                }
                
                ctx.controller_client.delete_kubernetes_cluster(name, true).await?;
                println!("✅ Cluster {} deleted", name);
                Ok(())
            }
            
            KubernetesCommand::Health { name } => {
                let health = ctx.controller_client.get_kubernetes_cluster_health(name).await?;
                
                println!("Overall: {}", health.overall_health);
                println!("Control planes: {}/{}", health.ready_control_planes, health.total_control_planes);
                println!("Workers: {}/{}", health.ready_workers, health.total_workers);
                println!("etcd: {}", if health.etcd_healthy { "healthy" } else { "unhealthy" });
                println!("API server: {}", if health.api_server_reachable { "reachable" } else { "unreachable" });
                
                println!("\nNode health:");
                for node in &health.node_health {
                    println!("  {} - {} (kubelet: {}, etcd: {}, api: {})",
                        node.node_id,
                        node.kubernetes_status,
                        node.kubelet_active,
                        node.etcd_active,
                        node.api_server_active,
                    );
                }
                
                Ok(())
            }
        }
    }
}
```

## Implementation Phases

### Phase 1: Proto and Module (Week 1)
- [ ] Add all gRPC messages to `proto/controller.proto` and `proto/node.proto`
- [ ] Run `buf generate` to regenerate Rust code
- [ ] Create `modules/kcore-kubernetes.nix` with full HA + Calico support
- [ ] Create `tests/kubernetes-module-eval.nix` flake check
- [ ] Verify: `nix build .#checks.x86_64-linux.kubernetes-module-eval`

### Phase 2: Nix Code Generation (Week 1)
- [ ] Implement `generate_kubernetes_nix()` in `crates/controller/src/nix_codegen.rs`
- [ ] Unit tests for Nix escaping (`nix_escape` already exists from Ceph)
- [ ] CIDR validation helpers
- [ ] etcd initial-cluster string builder
- [ ] Tests: `cargo test -p kcore-controller -- kubernetes`

### Phase 3: SQLite Schema (Week 1)
- [ ] Migration: `kubernetes_clusters` table (name, spec JSON, generation, created_at)
- [ ] Migration: `kubernetes_cluster_status` table (name, phase, message, ready_cps, ready_workers)
- [ ] Index on name
- [ ] Verify: `sqlx migrate run`

### Phase 4: Node Agent (Week 2)
- [ ] Implement `ApplyKubernetesConfig` RPC
- [ ] Implement `GetKubernetesStatus` RPC  
- [ ] Implement `DrainKubernetesNode` RPC
- [ ] Preflight checks (interface exists, ports free, CIDR non-overlapping)
- [ ] `nixos-rebuild test` then `switch` under apply lock
- [ ] Service verification (systemctl is-active)
- [ ] Tests: fake controller, verify Nix writes

### Phase 5: Controller Reconciler (Week 2-3)
- [ ] `PlanKubernetesCluster` - call node preflights in parallel
- [ ] `CreateKubernetesCluster`:
  - [ ] Bootstrap first control plane (easyCerts = true)
  - [ ] Wait for API server ready
  - [ ] Retrieve kubeconfig
  - [ ] Join additional control planes
  - [ ] Wait for etcd quorum
  - [ ] Join workers
  - [ ] Wait for Calico ready
  - [ ] Persist to DB
- [ ] `UpdateKubernetesCluster` - rolling update control planes first, then workers
- [ ] `DeleteKubernetesCluster` - render enable=false, wait for units down
- [ ] `ScaleKubernetesCluster` - add/remove nodes
- [ ] `GetKubernetesClusterHealth` - query all nodes via gRPC
- [ ] Tests: mock node clients

### Phase 6: CLI (Week 3)
- [ ] `kctl kubernetes plan -f cluster.yaml`
- [ ] `kctl kubernetes apply -f cluster.yaml`
- [ ] `kctl kubernetes get <name>`
- [ ] `kctl kubernetes list`
- [ ] `kctl kubernetes delete <name> --confirm`
- [ ] `kctl kubernetes health <name>`
- [ ] YAML spec loading and validation

### Phase 7: MCP Integration (Week 3)
- [ ] Add `kubernetes-cluster` kind to `kcore-mcp/packages/server/src/catalog.ts`
- [ ] Advise questions (node IDs, addresses, CIDRs)
- [ ] Plan path (call controller, format output)
- [ ] Apply path (confirm=true gating)
- [ ] Tests: `pnpm test` in kcore-mcp

### Phase 8: Lab Deployment (Week 4)
**Prerequisites:**
- [ ] Operator confirms node assignments
- [ ] Operator confirms static IPs on management network
- [ ] Operator confirms VIP address (192.168.40.200) is free
- [ ] Operator confirms no running VMs need to stay on these nodes

**Execution:**
- [ ] Drain VMs from 6 nodes
- [ ] `kctl kubernetes plan -f lab-ha.yaml`
- [ ] Review plan output with operator
- [ ] `kctl kubernetes apply -f lab-ha.yaml` with confirmation
- [ ] Monitor apply logs (follow controller logs)
- [ ] Verify: `kubectl --kubeconfig=/var/lib/kubernetes/admin.conf get nodes` shows 6 nodes Ready
- [ ] Verify: `kubectl get pods -A` shows all system pods Running
- [ ] Deploy test workload: `kubectl create deployment nginx --image=nginx --replicas=3`
- [ ] Verify pods spread across workers

### Phase 9: Testing & Validation (Week 4)
- [ ] Test control plane failover (stop one apiserver, verify VIP stays up)
- [ ] Test etcd quorum (stop 1 etcd, verify cluster still works)
- [ ] Test worker drain and re-add
- [ ] Test rolling update (change MTU, apply, verify no downtime)
- [ ] Test Calico NetworkPolicy enforcement
- [ ] Load test (many pods, verify CNI performance)
- [ ] Failover test (hard reboot control plane node, verify recovery)

### Phase 10: Documentation (Week 5)
- [ ] Operator guide: how to plan and apply
- [ ] Architecture diagram (control planes, workers, VIP, Calico)
- [ ] Troubleshooting guide (logs, service status checks)
- [ ] Update main README with Kubernetes section
- [ ] MCP documentation for Cursor agent usage

## Risks and Mitigations

| Risk | Mitigation |
|------|-----------|
| NixOS rebuild breaks running VMs | Apply lock serializes with VM/Ceph operations. Plan stage requires operator to drain nodes. |
| Calico conflicts with kcore VXLAN | Calico uses UDP 4789, kcore uses same on different bridges. Firewall rules isolate them. |
| etcd quorum loss during deploy | Deploy control planes sequentially. Bootstrap first, then join one at a time. |
| VIP failover delay | keepalived detects failure in <5s. API clients retry automatically. |
| Control plane on node-107 (controller host) | Operator must explicitly add it. Default excludes controller from cluster. |
| Pod CIDR exhaustion | Start with /16 (65k pods). Monitor with Prometheus alert. |
| PKI management complexity | NixOS easyCerts handles it on bootstrap. Additional nodes get certs from bootstrap. |
| CNI MTU issues | Default 1450 leaves room for VXLAN. Preflight warns if NIC MTU < 1500. |

## Blocking Questions Before Phase 8

1. **Which 6 nodes?** Proposed: control planes (node-105, node-107, node-119), workers (node-103, worker-2, worker-3)
2. **Static IPs confirmed?** Must be DHCP reservations or static assignments
3. **VIP address?** Proposed: 192.168.40.200 (must be free)
4. **Include node-107 (controller)?** Recommendation: No, keep controller separate initially
5. **MTU on management NICs?** Must be ≥1500 for VXLAN overhead
6. **Existing VMs on these nodes?** Must be drained first
7. **Approval for nixos-rebuild on 6 production nodes?** This is a one-way operation
8. **Backup strategy?** Controller DB backup before apply, etcd backup after bootstrap

## Success Criteria

- [ ] 3 control plane nodes Ready in `kubectl get nodes`
- [ ] 3 worker nodes Ready in `kubectl get nodes`
- [ ] All system pods Running (kube-apiserver, etcd, kubelet, calico-node, coredns)
- [ ] `kubectl create deployment test --image=nginx --replicas=6` spreads pods across all workers
- [ ] NetworkPolicy enforcement working (test with deny-all policy)
- [ ] VIP failover works (stop keepalived on master, VIP moves to backup in <5s)
- [ ] `kctl kubernetes health lab-ha` shows "healthy"
- [ ] etcd has 3 members in quorum: `kubectl exec -n kube-system etcd-<node> -- etcdctl member list`

## Future Enhancements (Post-MVP)

- [ ] CSI driver (Ceph RBD if SAN cluster exists, or hostPath provisioner)
- [ ] Ingress controller (nginx or traefik)
- [ ] Metrics (Prometheus operator)
- [ ] Logging (Loki)
- [ ] Cert-manager for automatic TLS
- [ ] Cilium as alternative CNI (eBPF-based)
- [ ] Calico BGP mode (peer with datacenter routers)
- [ ] Multi-cluster federation
- [ ] GPU scheduling (extend to pass-through GPUs to pods via device plugin)

---

**Next Steps:**
1. Review this plan with operator
2. Get approval for node assignments and VIP
3. Begin Phase 1 (proto + module)
4. Target: Phase 8 (lab deploy) in 4 weeks
