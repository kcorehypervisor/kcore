// Test program to create a Kubernetes cluster via database
use anyhow::Result;
use std::sync::Arc;
use std::collections::HashMap;

mod db;
mod kubernetes_orchestrator;
mod kubernetes_pki;
mod node_client;

#[tokio::main]
async fn main() -> Result<()> {
    // Open database (controller's database location)
    let db = db::Database::open("/var/lib/kcore/controller/kcore.db")?;
    
    // Insert kubernetes cluster
    db.lock_conn()?.execute(
        "INSERT INTO kubernetes_clusters (
            name, generation, cluster_cidr, service_cidr, cluster_dns,
            api_server_endpoint, cni, phase, status_message,
            ready_control_planes, ready_workers
        ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
        rusqlite::params![
            "test-k8s-135",
            1,
            "10.244.0.0/16",
            "10.96.0.0/12",
            "10.96.0.10",
            "192.168.40.200:6443",
            "cilium",
            "pending",
            "awaiting bootstrap",
            0,
            0
        ],
    )?;
    
    // Insert cluster config
    db.lock_conn()?.execute(
        "INSERT INTO kubernetes_cluster_config (
            cluster_name, ha_enabled, vip_address, vip_interface,
            cilium_mode, cilium_mtu, cilium_tunnel,
            cilium_auto_direct_routes, cilium_lb_ip_pools
        ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
        rusqlite::params![
            "test-k8s-135",
            1,
            "192.168.40.200",
            "enp6s0",
            "native-routing",
            1500,
            "disabled",
            1,
            ""
        ],
    )?;
    
    // Insert control plane nodes
    for (node_id, ip, initial) in [
        ("node-105", "192.168.40.105", 1),
        ("node-107", "192.168.40.107", 0),
        ("node-119", "192.168.40.119", 0),
    ] {
        db.lock_conn()?.execute(
            "INSERT INTO kubernetes_cluster_nodes (
                cluster_name, node_id, role, advertise_address, iface,
                initial_cluster, join_existing, kubelet_ready
            ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            rusqlite::params![
                "test-k8s-135",
                node_id,
                "control-plane",
                ip,
                "enp6s0",
                initial,
                0,
                0
            ],
        )?;
    }
    
    println!("✅ Kubernetes cluster 'test-k8s-135' created in database");
    println!("📊 Phase: pending (orchestrator should pick it up)");
    
    Ok(())
}
