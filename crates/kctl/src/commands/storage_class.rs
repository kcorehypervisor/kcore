use crate::client::{self, controller_proto};
use crate::config::ConnectionInfo;
use anyhow::{bail, Result};
use std::collections::BTreeMap;

pub async fn list(info: &ConnectionInfo) -> Result<()> {
    let mut client = client::controller_client(info).await?;
    let nodes = client
        .list_nodes(controller_proto::ListNodesRequest {})
        .await?
        .into_inner()
        .nodes;

    if nodes.is_empty() {
        println!("No nodes found");
        return Ok(());
    }

    let mut by_class: BTreeMap<&'static str, Vec<String>> = BTreeMap::new();
    for node in nodes {
        by_class
            .entry(storage_class_name(node.storage_backend))
            .or_default()
            .push(node.node_id);
    }

    println!("Storage classes:");
    for (class, node_ids) in by_class {
        println!("  - {:<12} nodes={}", class, node_ids.len());
    }
    Ok(())
}

pub async fn describe(info: &ConnectionInfo, storage_class: &str) -> Result<()> {
    let class = normalize_storage_class(storage_class)?;
    let mut client = client::controller_client(info).await?;
    let nodes = client
        .list_nodes(controller_proto::ListNodesRequest {})
        .await?
        .into_inner()
        .nodes;

    if nodes.is_empty() {
        println!("No nodes found");
        return Ok(());
    }

    let matches: Vec<_> = nodes
        .into_iter()
        .filter(|n| storage_class_name(n.storage_backend) == class)
        .collect();

    println!("Storage class: {class}");
    println!("Node count:    {}", matches.len());
    if matches.is_empty() {
        println!("Nodes:         (none)");
        return Ok(());
    }

    println!("Nodes:");
    for n in matches {
        println!(
            "  - {}  host={}  addr={}  status={}  approval={}",
            n.node_id, n.hostname, n.address, n.status, n.approval_status
        );
    }
    Ok(())
}

fn normalize_storage_class(value: &str) -> Result<&'static str> {
    let normalized = value.trim().to_ascii_lowercase();
    match normalized.as_str() {
        "filesystem" | "fs" => Ok("filesystem"),
        "lvm" => Ok("lvm"),
        "zfs" => Ok("zfs"),
        "unspecified" | "unknown" => Ok("unspecified"),
        _ => bail!("storage-class must be one of: filesystem, lvm, zfs, unspecified"),
    }
}

fn storage_class_name(value: i32) -> &'static str {
    match controller_proto::StorageBackendType::try_from(value)
        .unwrap_or(controller_proto::StorageBackendType::Unspecified)
    {
        controller_proto::StorageBackendType::Filesystem => "filesystem",
        controller_proto::StorageBackendType::Lvm => "lvm",
        controller_proto::StorageBackendType::Zfs => "zfs",
        controller_proto::StorageBackendType::Unspecified => "unspecified",
    }
}

#[cfg(test)]
mod tests {
    use super::normalize_storage_class;

    #[test]
    fn normalize_storage_class_accepts_expected_values() {
        assert_eq!(normalize_storage_class("filesystem").expect("filesystem"), "filesystem");
        assert_eq!(normalize_storage_class("fs").expect("fs"), "filesystem");
        assert_eq!(normalize_storage_class("lvm").expect("lvm"), "lvm");
        assert_eq!(normalize_storage_class("zfs").expect("zfs"), "zfs");
        assert_eq!(
            normalize_storage_class("unknown").expect("unknown"),
            "unspecified"
        );
        assert!(normalize_storage_class("ceph").is_err());
    }
}
