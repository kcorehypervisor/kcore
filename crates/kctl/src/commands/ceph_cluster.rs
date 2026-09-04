//! `kctl` subcommands for the [`CephCluster`] controller resource (kcore SAN).

use anyhow::{bail, Context, Result};
use serde::Deserialize;

use crate::apply_summary::render_apply_summary;
use crate::client::{self, controller_proto};
use crate::config::ConnectionInfo;

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct CephClusterManifest {
    kind: String,
    metadata: ManifestMetadata,
    spec: ManifestSpec,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ManifestMetadata {
    name: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ManifestSpec {
    #[serde(default)]
    fsid: String,
    public_network: String,
    cluster_network: String,
    #[serde(default)]
    replication: ManifestReplication,
    #[serde(default)]
    force_wipe: bool,
    nodes: Vec<ManifestNode>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ManifestReplication {
    #[serde(default = "default_size")]
    size: i32,
    #[serde(default = "default_min_size")]
    min_size: i32,
}

impl Default for ManifestReplication {
    fn default() -> Self {
        Self {
            size: default_size(),
            min_size: default_min_size(),
        }
    }
}

fn default_size() -> i32 {
    3
}
fn default_min_size() -> i32 {
    2
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ManifestNode {
    node_id: String,
    mon_addr: String,
    cluster_addr: String,
    public_iface: String,
    cluster_iface: String,
    osd_device: String,
}

fn parse_manifest(file: &str) -> Result<CephClusterManifest> {
    let data = std::fs::read_to_string(file).with_context(|| format!("reading {file}"))?;
    let manifest: CephClusterManifest =
        serde_yaml::from_str(&data).with_context(|| format!("parsing YAML in {file}"))?;
    let kind = manifest.kind.trim().to_ascii_lowercase();
    if kind != "cephcluster" && kind != "ceph-cluster" && kind != "ceph_cluster" {
        bail!("expected kind: CephCluster, got {}", manifest.kind);
    }
    Ok(manifest)
}

fn to_proto_spec(spec: &ManifestSpec) -> controller_proto::CephClusterSpec {
    controller_proto::CephClusterSpec {
        fsid: spec.fsid.clone(),
        public_network: spec.public_network.clone(),
        cluster_network: spec.cluster_network.clone(),
        size: spec.replication.size,
        min_size: spec.replication.min_size,
        force_wipe: spec.force_wipe,
        nodes: spec
            .nodes
            .iter()
            .map(|n| controller_proto::CephClusterNodeSpec {
                node_id: n.node_id.clone(),
                mon_addr: n.mon_addr.clone(),
                cluster_addr: n.cluster_addr.clone(),
                public_iface: n.public_iface.clone(),
                cluster_iface: n.cluster_iface.clone(),
                osd_device: n.osd_device.clone(),
            })
            .collect(),
    }
}

pub async fn apply_from_file(info: &ConnectionInfo, file: &str) -> Result<()> {
    let manifest = parse_manifest(file)?;
    let mut client = client::controller_client(info).await?;
    let resp = client
        .create_ceph_cluster(controller_proto::CreateCephClusterRequest {
            ceph_cluster: Some(controller_proto::CephCluster {
                name: manifest.metadata.name.clone(),
                generation: 0,
                spec: Some(to_proto_spec(&manifest.spec)),
                status: None,
                created_at: None,
                updated_at: None,
            }),
        })
        .await
        .context("create_ceph_cluster rpc")?
        .into_inner();

    let label = format!("ceph cluster '{}'", manifest.metadata.name);
    println!(
        "{}",
        render_apply_summary(resp.action, &resp.changed_fields, &label)
    );
    Ok(())
}

pub async fn list(info: &ConnectionInfo) -> Result<()> {
    let mut client = client::controller_client(info).await?;
    let resp = client
        .list_ceph_clusters(controller_proto::ListCephClustersRequest {})
        .await?
        .into_inner();
    if resp.ceph_clusters.is_empty() {
        println!("No Ceph clusters found");
        return Ok(());
    }
    println!("{:<24}  {:>4}  {:<14}  HEALTH", "NAME", "GEN", "PHASE");
    for cluster in resp.ceph_clusters {
        let status = cluster.status.unwrap_or_default();
        let phase = controller_proto::CephClusterPhase::try_from(status.phase)
            .unwrap_or(controller_proto::CephClusterPhase::Unspecified);
        println!(
            "{:<24}  {:>4}  {:<14}  {}",
            cluster.name,
            cluster.generation,
            format!("{phase:?}").replace("CephClusterPhase", ""),
            status.health_message
        );
    }
    Ok(())
}

pub async fn get(info: &ConnectionInfo, name: &str) -> Result<()> {
    let mut client = client::controller_client(info).await?;
    let resp = client
        .get_ceph_cluster(controller_proto::GetCephClusterRequest {
            name: name.to_string(),
        })
        .await?
        .into_inner();
    let cluster = resp
        .ceph_cluster
        .ok_or_else(|| anyhow::anyhow!("empty response"))?;
    let status = cluster.status.unwrap_or_default();
    let spec = cluster.spec.unwrap_or_default();
    println!("Name:       {}", cluster.name);
    println!("Generation: {}", cluster.generation);
    println!("FSID:       {}", spec.fsid);
    println!("Public:     {}", spec.public_network);
    println!("Cluster:    {}", spec.cluster_network);
    println!("Size/min:   {}/{}", spec.size, spec.min_size);
    println!(
        "Phase:      {:?}",
        controller_proto::CephClusterPhase::try_from(status.phase)
            .unwrap_or(controller_proto::CephClusterPhase::Unspecified)
    );
    println!("Health:     {}", status.health_message);
    println!("Nodes:");
    for n in &spec.nodes {
        println!(
            "  - {} mon={} cluster={} osd={}",
            n.node_id, n.mon_addr, n.cluster_addr, n.osd_device
        );
    }
    Ok(())
}

pub async fn delete(info: &ConnectionInfo, name: &str) -> Result<()> {
    let mut client = client::controller_client(info).await?;
    let resp = client
        .delete_ceph_cluster(controller_proto::DeleteCephClusterRequest {
            name: name.to_string(),
        })
        .await?
        .into_inner();
    if resp.success {
        println!("Deleted Ceph cluster '{name}'");
    } else {
        bail!("failed to delete Ceph cluster '{name}'");
    }
    Ok(())
}
