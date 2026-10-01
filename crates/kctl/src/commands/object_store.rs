//! `kctl` subcommands for [`ObjectStore`] (RGW).

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
    members: Vec<String>,
    #[serde(default)]
    port: i32,
    #[serde(default)]
    tls: bool,
    #[serde(default)]
    rgw_id: String,
}

fn parse_manifest(file: &str) -> Result<Manifest> {
    let data = std::fs::read_to_string(file).with_context(|| format!("reading {file}"))?;
    let manifest: Manifest =
        serde_yaml::from_str(&data).with_context(|| format!("parsing YAML in {file}"))?;
    let kind = manifest.kind.trim().to_ascii_lowercase();
    if kind != "objectstore" && kind != "object-store" && kind != "object_store" {
        bail!("expected kind: ObjectStore, got {}", manifest.kind);
    }
    Ok(manifest)
}

fn to_proto_spec(spec: &ManifestSpec) -> controller_proto::ObjectStoreSpec {
    controller_proto::ObjectStoreSpec {
        ceph_cluster: spec.ceph_cluster.trim().to_string(),
        members: spec.members.iter().map(|m| m.trim().to_string()).collect(),
        port: spec.port,
        tls: spec.tls,
        rgw_id: spec.rgw_id.trim().to_string(),
    }
}

pub async fn apply_from_file(info: &ConnectionInfo, file: &str) -> Result<()> {
    let manifest = parse_manifest(file)?;
    let mut client = client::controller_client(info).await?;
    let resp = client
        .create_object_store(controller_proto::CreateObjectStoreRequest {
            object_store: Some(controller_proto::ObjectStore {
                name: manifest.metadata.name.clone(),
                generation: 0,
                spec: Some(to_proto_spec(&manifest.spec)),
                status: None,
                created_at: None,
                updated_at: None,
            }),
        })
        .await
        .context("create_object_store rpc")?
        .into_inner();
    let label = format!("ObjectStore '{}'", manifest.metadata.name);
    println!(
        "{}",
        render_apply_summary(resp.action, &resp.changed_fields, &label)
    );
    Ok(())
}

pub async fn get(info: &ConnectionInfo, name: &str) -> Result<()> {
    let mut client = client::controller_client(info).await?;
    let resp = client
        .get_object_store(controller_proto::GetObjectStoreRequest {
            name: name.to_string(),
        })
        .await
        .context("get_object_store rpc")?
        .into_inner();
    let store = resp
        .object_store
        .ok_or_else(|| anyhow::anyhow!("empty response"))?;
    let status = store.status.unwrap_or_default();
    let spec = store.spec.unwrap_or_default();
    println!("Name:         {}", store.name);
    println!("Generation:   {}", store.generation);
    println!("CephCluster:  {}", spec.ceph_cluster);
    println!("Port:         {}", spec.port);
    println!("TLS:          {}", spec.tls);
    println!("RgwId:        {}", spec.rgw_id);
    println!(
        "Phase:        {:?}",
        controller_proto::ObjectStorePhase::try_from(status.phase)
            .unwrap_or(controller_proto::ObjectStorePhase::Unspecified)
    );
    println!("Health:       {}", status.health_message);
    println!("Members:      {}", spec.members.join(", "));
    Ok(())
}

pub async fn list(info: &ConnectionInfo) -> Result<()> {
    let mut client = client::controller_client(info).await?;
    let resp = client
        .list_object_stores(controller_proto::ListObjectStoresRequest {})
        .await
        .context("list_object_stores rpc")?
        .into_inner();
    if resp.object_stores.is_empty() {
        println!("No ObjectStore resources.");
        return Ok(());
    }
    for store in resp.object_stores {
        let status = store.status.unwrap_or_default();
        let phase = controller_proto::ObjectStorePhase::try_from(status.phase)
            .unwrap_or(controller_proto::ObjectStorePhase::Unspecified);
        println!(
            "{}  gen={}  phase={phase:?}  {}",
            store.name, store.generation, status.health_message
        );
    }
    Ok(())
}

pub async fn delete(info: &ConnectionInfo, name: &str) -> Result<()> {
    let mut client = client::controller_client(info).await?;
    let resp = client
        .delete_object_store(controller_proto::DeleteObjectStoreRequest {
            name: name.to_string(),
        })
        .await
        .context("delete_object_store rpc")?
        .into_inner();
    if resp.success {
        println!("Deleted ObjectStore '{name}'.");
    } else {
        bail!("ObjectStore '{name}' was not found");
    }
    Ok(())
}
