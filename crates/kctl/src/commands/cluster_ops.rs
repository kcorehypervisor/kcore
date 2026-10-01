use std::path::Path;

use crate::client::{self, controller_proto};
use crate::config::ConnectionInfo;
use anyhow::{Context, Result};

pub async fn health(info: &ConnectionInfo) -> Result<()> {
    let mut client = client::controller_client(info).await?;
    let resp = client
        .get_cluster_health(controller_proto::GetClusterHealthRequest {})
        .await?
        .into_inner();
    println!("status: {}", resp.status);
    println!(
        "nodes: {} ready, {} not-ready, {} unschedulable, {} pending",
        resp.ready_nodes, resp.not_ready_nodes, resp.unschedulable_nodes, resp.pending_nodes
    );
    println!(
        "vms: {} total, {} on not-ready nodes",
        resp.vm_count, resp.vms_on_not_ready_nodes
    );
    println!("replication conflicts: {}", resp.replication_conflicts);
    println!(
        "certificates expiring soon: {}",
        resp.certificates_expiring_soon
    );
    if resp.conditions.is_empty() {
        println!("conditions: none");
    } else {
        println!("conditions:");
        for condition in resp.conditions {
            println!("  - {condition}");
        }
    }
    Ok(())
}

pub async fn backup(info: &ConnectionInfo, output: &Path) -> Result<()> {
    let mut client = client::controller_client(info).await?;
    let resp = client
        .backup_cluster(controller_proto::BackupClusterRequest {})
        .await?
        .into_inner();
    if let Some(parent) = output.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating {}", parent.display()))?;
        }
    }
    std::fs::write(output, &resp.sqlite)
        .with_context(|| format!("writing {}", output.display()))?;
    println!(
        "wrote {} bytes (schema {}) to {}",
        resp.sqlite.len(),
        resp.schema_version,
        output.display()
    );
    Ok(())
}

pub async fn restore(info: &ConnectionInfo, file: &Path, confirm: bool) -> Result<()> {
    if !confirm {
        anyhow::bail!("restore replaces the controller database; pass --confirm");
    }
    let sqlite = std::fs::read(file).with_context(|| format!("reading {}", file.display()))?;
    let mut client = client::controller_client(info).await?;
    let resp = client
        .restore_cluster(controller_proto::RestoreClusterRequest {
            sqlite,
            confirm: true,
        })
        .await?
        .into_inner();
    if resp.success {
        println!("{}", resp.message);
        Ok(())
    } else {
        anyhow::bail!("restore failed: {}", resp.message)
    }
}
