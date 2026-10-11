// Kubernetes NixOS configuration generation for node-agent
//
// This module generates the kcore.kubernetes module configuration
// that gets written to /etc/nixos/kcore-kubernetes.nix

use kcore_sanitize::nix_escape;

#[derive(Debug, Clone)]
pub struct KubernetesNodeConfig {
    pub role: String,
    pub cluster_name: String,
    pub cluster_cidr: String,
    pub service_cidr: String,
    pub cluster_dns: String,
    pub advertise_address: String,
    pub iface: String,
    pub api_server_endpoint: String,
    pub initial_cluster: bool,
    pub etcd_initial_cluster: String,
    pub cni: String,
    pub cilium_mode: String,
    pub cilium_mtu: i32,
    pub cilium_tunnel: String,
    pub cilium_auto_direct_routes: bool,
    pub cilium_lb_ip_pools: Vec<String>,
    pub ha_enabled: bool,
    pub vip_address: String,
    pub vip_interface: String,
    pub kubeconfig_data: String,
}

pub fn generate_kubernetes_node_config(k8s: &KubernetesNodeConfig) -> String {
    let mut out = String::new();
    out.push_str("{\n");
    out.push_str("  kcore.kubernetes = {\n");
    out.push_str("    enable = true;\n");
    out.push_str(&format!("    role = \"{}\";\n", nix_escape(&k8s.role)));
    out.push_str(&format!("    clusterName = \"{}\";\n", nix_escape(&k8s.cluster_name)));
    out.push_str(&format!("    clusterCidr = \"{}\";\n", nix_escape(&k8s.cluster_cidr)));
    out.push_str(&format!("    serviceCidr = \"{}\";\n", nix_escape(&k8s.service_cidr)));
    out.push_str(&format!("    clusterDns = \"{}\";\n", nix_escape(&k8s.cluster_dns)));
    out.push_str(&format!("    advertiseAddress = \"{}\";\n", nix_escape(&k8s.advertise_address)));
    out.push_str(&format!("    iface = \"{}\";\n", nix_escape(&k8s.iface)));
    out.push_str(&format!("    apiServerEndpoint = \"{}\";\n", nix_escape(&k8s.api_server_endpoint)));
    out.push_str(&format!("    initialCluster = {};\n", k8s.initial_cluster));
    
    if k8s.role == "control-plane" {
        out.push_str(&format!("    etcdInitialCluster = \"{}\";\n", nix_escape(&k8s.etcd_initial_cluster)));
    }
    
    out.push_str(&format!("    cni = \"{}\";\n", nix_escape(&k8s.cni)));
    out.push_str(&format!("    ciliumMode = \"{}\";\n", nix_escape(&k8s.cilium_mode)));
    out.push_str(&format!("    ciliumMtu = {};\n", k8s.cilium_mtu));
    out.push_str(&format!("    ciliumTunnel = \"{}\";\n", nix_escape(&k8s.cilium_tunnel)));
    out.push_str(&format!("    ciliumAutoDirectNodeRoutes = {};\n", k8s.cilium_auto_direct_routes));
    
    if !k8s.cilium_lb_ip_pools.is_empty() {
        out.push_str("    ciliumLoadBalancerIpPools = [\n");
        for pool in &k8s.cilium_lb_ip_pools {
            out.push_str(&format!("      \"{}\"\n", nix_escape(pool)));
        }
        out.push_str("    ];\n");
    }
    
    out.push_str(&format!("    haEnabled = {};\n", k8s.ha_enabled));
    if k8s.ha_enabled {
        out.push_str(&format!("    vipAddress = \"{}\";\n", nix_escape(&k8s.vip_address)));
        out.push_str(&format!("    vipInterface = \"{}\";\n", nix_escape(&k8s.vip_interface)));
    }
    
    if !k8s.kubeconfig_data.is_empty() {
        out.push_str(&format!("    kubeconfigData = ''{}'';\n", k8s.kubeconfig_data));
    }
    
    out.push_str("  };\n");
    out.push_str("}\n");
    out
}
