//! `kctl` subcommands for SlurmCluster.

use anyhow::{bail, Context, Result};
use serde::Deserialize;

use crate::client::{self, controller_proto};
use crate::config::ConnectionInfo;

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct SlurmClusterManifest {
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
    cluster_name: String,
    controller: ManifestController,
    #[serde(default)]
    backup_controller: Option<ManifestController>,
    #[serde(default)]
    login: ManifestLogin,
    #[serde(default)]
    accounting: ManifestAccounting,
    #[serde(default)]
    compute: Vec<ManifestCompute>,
    #[serde(default)]
    partitions: Vec<ManifestPartition>,
    #[serde(default)]
    users: Vec<ManifestUser>,
    #[serde(default)]
    storage: ManifestStorage,
    #[serde(default)]
    extra_config: String,
    #[serde(default)]
    allow_controller_host: bool,
    #[serde(default)]
    gpu_driver: bool,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ManifestController {
    node_id: String,
    address: String,
    iface: String,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ManifestLogin {
    #[serde(default)]
    node_ids: Vec<String>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ManifestAccounting {
    #[serde(default)]
    enabled: bool,
    #[serde(default)]
    node_id: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ManifestCompute {
    node_id: String,
    address: String,
    iface: String,
    #[serde(default)]
    partitions: Vec<String>,
    #[serde(default)]
    reserve: ManifestReserve,
    #[serde(default = "default_gres")]
    gres: String,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ManifestReserve {
    #[serde(default)]
    cores: i32,
    #[serde(default)]
    memory_mb: i64,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ManifestPartition {
    name: String,
    #[serde(default)]
    default: bool,
    #[serde(default)]
    max_time: String,
    #[serde(default)]
    state: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ManifestUser {
    name: String,
    uid: u32,
    #[serde(default)]
    ssh_keys: Vec<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ManifestStorage {
    #[serde(default = "default_storage_mode")]
    mode: String,
    #[serde(default = "default_exports")]
    exports: Vec<String>,
    #[serde(default)]
    filesystem: String,
}

impl Default for ManifestStorage {
    fn default() -> Self {
        Self {
            mode: default_storage_mode(),
            exports: default_exports(),
            filesystem: String::new(),
        }
    }
}

fn default_gres() -> String {
    "none".into()
}

fn default_storage_mode() -> String {
    "nfs".into()
}

fn default_exports() -> Vec<String> {
    vec!["/home".into(), "/scratch".into()]
}

fn parse_manifest(file: &str) -> Result<SlurmClusterManifest> {
    let data = std::fs::read_to_string(file).with_context(|| format!("reading {file}"))?;
    parse_manifest_str(&data)
}

fn parse_manifest_str(data: &str) -> Result<SlurmClusterManifest> {
    let manifest: SlurmClusterManifest = serde_yaml::from_str(data).context("parsing YAML")?;
    let kind = manifest.kind.trim().to_ascii_lowercase();
    if kind != "slurmcluster" && kind != "slurm-cluster" && kind != "slurm_cluster" {
        bail!("expected kind: SlurmCluster, got {}", manifest.kind);
    }
    Ok(manifest)
}

fn controller(spec: &ManifestController) -> controller_proto::SlurmControllerSpec {
    controller_proto::SlurmControllerSpec {
        node_id: spec.node_id.clone(),
        address: spec.address.clone(),
        iface: spec.iface.clone(),
    }
}

fn to_proto_spec(spec: &ManifestSpec) -> controller_proto::SlurmClusterSpec {
    controller_proto::SlurmClusterSpec {
        cluster_name: spec.cluster_name.clone(),
        controller: Some(controller(&spec.controller)),
        backup_controller: spec.backup_controller.as_ref().map(controller),
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

fn roles_for(spec: &ManifestSpec, node_id: &str) -> Vec<&'static str> {
    let mut roles = Vec::new();
    if spec.controller.node_id == node_id
        || spec
            .backup_controller
            .as_ref()
            .is_some_and(|b| b.node_id == node_id)
    {
        roles.push("controller");
    }
    if spec.accounting.enabled {
        let dbd = if spec.accounting.node_id.is_empty() {
            spec.controller.node_id.as_str()
        } else {
            spec.accounting.node_id.as_str()
        };
        if dbd == node_id {
            roles.push("dbd");
        }
    }
    if spec.compute.iter().any(|c| c.node_id == node_id) {
        roles.push("compute");
    }
    if spec.login.node_ids.iter().any(|n| n == node_id) {
        roles.push("login");
    }
    roles
}

fn plan_lines(
    name: &str,
    spec: &ManifestSpec,
    plan: &controller_proto::PlanSlurmClusterResponse,
) -> Vec<String> {
    let mut out = vec![format!("=== SlurmCluster plan: {name} ===")];
    out.push(format!("Action: {}", plan.action));
    if !plan.blocking_issues.is_empty() {
        out.push("\nBlocking issues:".into());
        for issue in &plan.blocking_issues {
            out.push(format!("  - {issue}"));
        }
    }
    if !plan.warnings.is_empty() {
        out.push("\nWarnings:".into());
        for warning in &plan.warnings {
            out.push(format!("  - {warning}"));
        }
    }
    out.push("\nNodes:".into());
    for node in &plan.node_plans {
        let roles = roles_for(spec, &node.node_id).join(",");
        out.push(format!(
            "  {} roles={} action={} {}",
            node.node_id,
            roles,
            node.nix_action,
            if node.preflight_passed { "ok" } else { "FAIL" }
        ));
        for issue in &node.issues {
            out.push(format!("      - {issue}"));
        }
    }
    out
}

fn print_plan(name: &str, spec: &ManifestSpec, plan: &controller_proto::PlanSlurmClusterResponse) {
    for line in plan_lines(name, spec, plan) {
        println!("{line}");
    }
}

pub async fn plan_from_file(info: &ConnectionInfo, file: &str) -> Result<()> {
    let manifest = parse_manifest(file)?;
    let mut client = client::controller_client(info).await?;
    let plan = client
        .plan_slurm_cluster(controller_proto::PlanSlurmClusterRequest {
            name: manifest.metadata.name.clone(),
            spec: Some(to_proto_spec(&manifest.spec)),
        })
        .await
        .context("plan_slurm_cluster rpc")?
        .into_inner();
    print_plan(&manifest.metadata.name, &manifest.spec, &plan);
    if !plan.blocking_issues.is_empty() {
        bail!("plan has {} blocking issue(s)", plan.blocking_issues.len());
    }
    Ok(())
}

pub async fn apply_from_file(info: &ConnectionInfo, file: &str) -> Result<()> {
    let manifest = parse_manifest(file)?;
    let mut client = client::controller_client(info).await?;
    let plan = client
        .plan_slurm_cluster(controller_proto::PlanSlurmClusterRequest {
            name: manifest.metadata.name.clone(),
            spec: Some(to_proto_spec(&manifest.spec)),
        })
        .await
        .context("plan_slurm_cluster rpc")?
        .into_inner();
    print_plan(&manifest.metadata.name, &manifest.spec, &plan);
    if !plan.blocking_issues.is_empty() {
        bail!(
            "cannot apply: {} blocking issue(s)",
            plan.blocking_issues.len()
        );
    }
    let response = client
        .create_slurm_cluster(controller_proto::CreateSlurmClusterRequest {
            name: manifest.metadata.name.clone(),
            spec: Some(to_proto_spec(&manifest.spec)),
            confirm: true,
        })
        .await
        .context("create_slurm_cluster rpc")?
        .into_inner();
    println!(
        "{}",
        crate::apply_summary::render_apply_summary(
            response.action,
            &response.changed_fields,
            &format!("SlurmCluster '{}'", manifest.metadata.name)
        )
    );
    println!("{}", response.message);
    Ok(())
}

pub async fn list(info: &ConnectionInfo) -> Result<()> {
    let mut client = client::controller_client(info).await?;
    let response = client
        .list_slurm_clusters(controller_proto::ListSlurmClustersRequest {})
        .await
        .context("list_slurm_clusters rpc")?
        .into_inner();
    if response.clusters.is_empty() {
        println!("No SlurmClusters");
        return Ok(());
    }
    println!(
        "{:<24} {:<12} {:<14} {:<8} {:<8}",
        "NAME", "PHASE", "CONTROLLER", "COMPUTE", "GEN"
    );
    for cluster in response.clusters {
        let controller = cluster
            .spec
            .as_ref()
            .and_then(|s| s.controller.as_ref())
            .map(|c| c.node_id.clone())
            .unwrap_or_default();
        let compute = cluster.spec.as_ref().map(|s| s.compute.len()).unwrap_or(0);
        let phase = cluster
            .status
            .as_ref()
            .map(|s| s.phase.clone())
            .unwrap_or_default();
        println!(
            "{:<24} {:<12} {:<14} {:<8} {:<8}",
            cluster.name, phase, controller, compute, cluster.generation
        );
    }
    Ok(())
}

pub async fn get(info: &ConnectionInfo, name: &str) -> Result<()> {
    let mut client = client::controller_client(info).await?;
    let response = client
        .get_slurm_cluster(controller_proto::GetSlurmClusterRequest {
            name: name.to_string(),
        })
        .await
        .context("get_slurm_cluster rpc")?
        .into_inner();
    let cluster = response
        .cluster
        .ok_or_else(|| anyhow::anyhow!("SlurmCluster '{name}' not found"))?;
    print_cluster(&cluster);
    Ok(())
}

fn print_cluster(cluster: &controller_proto::SlurmCluster) {
    println!("SlurmCluster: {}", cluster.name);
    println!("Generation: {}", cluster.generation);
    if let Some(spec) = &cluster.spec {
        println!("Cluster name: {}", spec.cluster_name);
        if let Some(controller) = &spec.controller {
            println!(
                "Controller: {} ({}) iface {}",
                controller.node_id, controller.address, controller.iface
            );
        }
        if let Some(backup) = &spec.backup_controller {
            println!("Backup controller: {} ({})", backup.node_id, backup.address);
        }
        println!(
            "Storage: {}{}",
            spec.storage
                .as_ref()
                .map(|s| s.mode.as_str())
                .unwrap_or("nfs"),
            spec.storage
                .as_ref()
                .filter(|s| !s.filesystem.is_empty())
                .map(|s| format!(" ({})", s.filesystem))
                .unwrap_or_default()
        );
        if let Some(accounting) = &spec.accounting {
            println!(
                "Accounting: {}",
                if accounting.enabled { "on" } else { "off" }
            );
        }
    }
    if let Some(status) = &cluster.status {
        println!("Phase: {}", status.phase);
        println!("Message: {}", status.message);
        println!("Observed generation: {}", status.observed_generation);
        println!("Nodes:");
        for node in &status.nodes {
            println!(
                "  {} roles={} state={} applied_generation={}",
                node.node_id,
                node.roles.join(","),
                node.slurm_state,
                node.applied_generation
            );
        }
    }
}

pub async fn delete(info: &ConnectionInfo, name: &str, force: bool) -> Result<()> {
    let mut client = client::controller_client(info).await?;
    let response = client
        .delete_slurm_cluster(controller_proto::DeleteSlurmClusterRequest {
            name: name.to_string(),
            force,
        })
        .await
        .context("delete_slurm_cluster rpc")?
        .into_inner();
    println!("{}", response.message);
    Ok(())
}

pub async fn rotate_munge_key(info: &ConnectionInfo, name: &str) -> Result<()> {
    let mut client = client::controller_client(info).await?;
    let response = client
        .rotate_slurm_munge_key(controller_proto::RotateSlurmMungeKeyRequest {
            name: name.to_string(),
        })
        .await
        .context("rotate_slurm_munge_key rpc")?
        .into_inner();
    println!("{}", response.message);
    Ok(())
}

pub async fn drain_node(
    info: &ConnectionInfo,
    name: &str,
    node_id: &str,
    reason: &str,
) -> Result<()> {
    let mut client = client::controller_client(info).await?;
    let response = client
        .drain_slurm_node(controller_proto::DrainSlurmNodeRequest {
            name: name.to_string(),
            node_id: node_id.to_string(),
            reason: reason.to_string(),
        })
        .await
        .context("drain_slurm_node rpc")?
        .into_inner();
    println!("{}", response.message);
    Ok(())
}

pub async fn resume_node(info: &ConnectionInfo, name: &str, node_id: &str) -> Result<()> {
    let mut client = client::controller_client(info).await?;
    let response = client
        .resume_slurm_node(controller_proto::ResumeSlurmNodeRequest {
            name: name.to_string(),
            node_id: node_id.to_string(),
        })
        .await
        .context("resume_slurm_node rpc")?
        .into_inner();
    println!("{}", response.message);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_camel_case_manifest() {
        let manifest = parse_manifest_str(
            r#"
kind: SlurmCluster
metadata:
  name: hpc-a
spec:
  clusterName: hpc-a
  controller:
    nodeId: node-1
    address: 10.0.0.1
    iface: enp6s0
  login:
    nodeIds: [node-1]
  compute:
    - nodeId: node-2
      address: 10.0.0.2
      iface: enp6s0
      partitions: [batch]
      reserve: { cores: 2, memoryMb: 4096 }
  partitions:
    - name: batch
      default: true
      maxTime: INFINITE
      state: UP
  users:
    - name: alice
      uid: 2001
      sshKeys: [ssh-ed25519 AAAA]
  storage:
    mode: nfs
    exports: [/home, /scratch]
"#,
        )
        .unwrap();
        let proto = to_proto_spec(&manifest.spec);
        assert_eq!(proto.cluster_name, "hpc-a");
        assert_eq!(proto.compute[0].reserve.as_ref().unwrap().cores, 2);
        assert_eq!(roles_for(&manifest.spec, "node-2"), vec!["compute"]);
    }

    #[test]
    fn plan_output_lists_roles_and_action() {
        let manifest = parse_manifest_str(
            r#"
kind: SlurmCluster
metadata: { name: hpc-a }
spec:
  clusterName: hpc-a
  controller: { nodeId: node-1, address: 10.0.0.1, iface: enp6s0 }
  compute:
    - { nodeId: node-2, address: 10.0.0.2, iface: enp6s0, partitions: [batch] }
  partitions: [{ name: batch, default: true }]
"#,
        )
        .unwrap();
        let plan = controller_proto::PlanSlurmClusterResponse {
            action: "update".into(),
            blocking_issues: Vec::new(),
            warnings: Vec::new(),
            node_plans: vec![
                controller_proto::NodePlanResult {
                    node_id: "node-1".into(),
                    preflight_passed: true,
                    issues: Vec::new(),
                    nix_action: "rebuild".into(),
                },
                controller_proto::NodePlanResult {
                    node_id: "node-2".into(),
                    preflight_passed: true,
                    issues: Vec::new(),
                    nix_action: "rebuild".into(),
                },
            ],
            rendered_nix_preview: String::new(),
        };
        let text = plan_lines("hpc-a", &manifest.spec, &plan).join("\n");
        assert!(text.contains("Action: update"));
        assert!(text.contains("node-1 roles=controller action=rebuild"));
        assert!(text.contains("node-2 roles=compute action=rebuild"));
    }

    #[test]
    fn rejects_other_kinds() {
        assert!(
            parse_manifest_str("kind: KubernetesCluster\nmetadata: {name: x}\nspec: {}\n").is_err()
        );
    }
}
