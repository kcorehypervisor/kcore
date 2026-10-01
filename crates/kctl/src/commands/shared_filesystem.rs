//! `kctl` subcommands for [`SharedFilesystem`] (CephFS).

use anyhow::{bail, Context, Result};
use serde::Deserialize;

use crate::apply_summary::render_apply_summary;
use crate::client::{self, controller_proto};
use crate::config::ConnectionInfo;

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Manifest {
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
    ceph_cluster: String,
    #[serde(default)]
    quota_bytes: i64,
    #[serde(default)]
    fs_name: String,
    #[serde(default)]
    clients: Vec<ManifestClient>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ManifestClient {
    name: String,
    #[serde(default)]
    paths: Vec<String>,
}

fn parse_manifest(file: &str) -> Result<Manifest> {
    let data = std::fs::read_to_string(file).with_context(|| format!("reading {file}"))?;
    let manifest: Manifest =
        serde_yaml::from_str(&data).with_context(|| format!("parsing YAML in {file}"))?;
    let kind = manifest.kind.trim().to_ascii_lowercase();
    if kind != "sharedfilesystem" && kind != "shared-filesystem" && kind != "shared_filesystem" {
        bail!("expected kind: SharedFilesystem, got {}", manifest.kind);
    }
    Ok(manifest)
}

fn to_proto_spec(spec: &ManifestSpec) -> controller_proto::SharedFilesystemSpec {
    controller_proto::SharedFilesystemSpec {
        ceph_cluster: spec.ceph_cluster.trim().to_string(),
        quota_bytes: spec.quota_bytes,
        fs_name: spec.fs_name.trim().to_string(),
        clients: spec
            .clients
            .iter()
            .map(|c| controller_proto::SharedFilesystemClientSpec {
                name: c.name.trim().to_string(),
                paths: c.paths.iter().map(|p| p.trim().to_string()).collect(),
            })
            .collect(),
    }
}

pub async fn apply_from_file(info: &ConnectionInfo, file: &str) -> Result<()> {
    let manifest = parse_manifest(file)?;
    let mut client = client::controller_client(info).await?;
    let resp = client
        .create_shared_filesystem(controller_proto::CreateSharedFilesystemRequest {
            shared_filesystem: Some(controller_proto::SharedFilesystem {
                name: manifest.metadata.name.clone(),
                generation: 0,
                spec: Some(to_proto_spec(&manifest.spec)),
                status: None,
                created_at: None,
                updated_at: None,
            }),
        })
        .await
        .context("create_shared_filesystem rpc")?
        .into_inner();
    let label = format!("SharedFilesystem '{}'", manifest.metadata.name);
    println!(
        "{}",
        render_apply_summary(resp.action, &resp.changed_fields, &label)
    );
    Ok(())
}

pub async fn get(info: &ConnectionInfo, name: &str) -> Result<()> {
    let mut client = client::controller_client(info).await?;
    let resp = client
        .get_shared_filesystem(controller_proto::GetSharedFilesystemRequest {
            name: name.to_string(),
        })
        .await
        .context("get_shared_filesystem rpc")?
        .into_inner();
    let fs = resp
        .shared_filesystem
        .ok_or_else(|| anyhow::anyhow!("empty response"))?;
    let status = fs.status.unwrap_or_default();
    let spec = fs.spec.unwrap_or_default();
    println!("Name:         {}", fs.name);
    println!("Generation:   {}", fs.generation);
    println!("CephCluster:  {}", spec.ceph_cluster);
    println!("FsName:       {}", spec.fs_name);
    println!("QuotaBytes:   {}", spec.quota_bytes);
    println!(
        "Phase:        {:?}",
        controller_proto::SharedFilesystemPhase::try_from(status.phase)
            .unwrap_or(controller_proto::SharedFilesystemPhase::Unspecified)
    );
    println!("Health:       {}", status.health_message);
    if !spec.clients.is_empty() {
        println!("Clients:");
        for c in &spec.clients {
            println!("  - {} paths={:?}", c.name, c.paths);
        }
    }
    Ok(())
}

pub async fn list(info: &ConnectionInfo) -> Result<()> {
    let mut client = client::controller_client(info).await?;
    let resp = client
        .list_shared_filesystems(controller_proto::ListSharedFilesystemsRequest {})
        .await
        .context("list_shared_filesystems rpc")?
        .into_inner();
    if resp.shared_filesystems.is_empty() {
        println!("No SharedFilesystem resources.");
        return Ok(());
    }
    for fs in resp.shared_filesystems {
        let status = fs.status.unwrap_or_default();
        let phase = controller_proto::SharedFilesystemPhase::try_from(status.phase)
            .unwrap_or(controller_proto::SharedFilesystemPhase::Unspecified);
        println!(
            "{}  gen={}  phase={phase:?}  {}",
            fs.name, fs.generation, status.health_message
        );
    }
    Ok(())
}

pub async fn delete(info: &ConnectionInfo, name: &str) -> Result<()> {
    let mut client = client::controller_client(info).await?;
    let resp = client
        .delete_shared_filesystem(controller_proto::DeleteSharedFilesystemRequest {
            name: name.to_string(),
        })
        .await
        .context("delete_shared_filesystem rpc")?
        .into_inner();
    if resp.success {
        println!("Deleted SharedFilesystem '{name}'.");
    } else {
        bail!("SharedFilesystem '{name}' was not found");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_kind_aliases() {
        for kind in ["shared-filesystem", "shared_filesystem", "SharedFilesystem"] {
            let yaml =
                format!("kind: {kind}\nmetadata:\n  name: fs1\nspec:\n  cephCluster: ceph\n");
            let m: Manifest = serde_yaml::from_str(&yaml).unwrap();
            assert_eq!(m.metadata.name, "fs1");
        }
    }
}
