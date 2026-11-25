use std::io::{Seek, SeekFrom, Write};
use std::path::Path;

use crate::client::{self, controller_proto};
use crate::config::ConnectionInfo;
use crate::output;
use anyhow::{bail, Context, Result};
use tokio_stream::StreamExt;

pub async fn list(info: &ConnectionInfo, vm: Option<String>) -> Result<()> {
    let mut client = client::controller_client(info).await?;
    let resp = client
        .list_volumes(controller_proto::ListVolumesRequest {
            vm: vm.unwrap_or_default(),
        })
        .await?
        .into_inner();

    if resp.volumes.is_empty() {
        println!("No volumes found.");
    } else {
        output::print_volume_table(&resp.volumes);
    }
    Ok(())
}

pub async fn get(info: &ConnectionInfo, name: &str) -> Result<()> {
    let mut client = client::controller_client(info).await?;
    let resp = client
        .get_volume(controller_proto::GetVolumeRequest {
            name: name.to_string(),
        })
        .await?
        .into_inner();
    let Some(v) = resp.volume else {
        bail!("volume '{name}' not found");
    };
    println!("Name:         {}", v.name);
    println!("Id:           {}", v.id);
    println!("Role:         {}", v.role);
    println!("Attach:       {}", v.attach_state);
    println!("VM:           {} ({})", v.vm_name, v.vm_id);
    println!("Node:         {}", v.node_id);
    println!("Pool/image:   {}/{}", v.pool, v.image);
    println!("Size bytes:   {}", v.storage_size_bytes);
    let guest = if v.guest_visible_bytes < 0 {
        "unknown".to_string()
    } else {
        crate::client::format_bytes(v.guest_visible_bytes)
    };
    println!("Guest visible: {guest}");
    println!("Encrypted:    {}", v.encrypted);
    println!("Serial:       {}", v.serial);
    println!("Handle:       {}", v.backend_handle);
    Ok(())
}

pub async fn resize(
    info: &ConnectionInfo,
    name: &str,
    size_bytes: i64,
    grow_filesystem: bool,
    allow_shrink: bool,
) -> Result<()> {
    if size_bytes <= 0 {
        bail!("--size-bytes must be positive");
    }
    let mut client = client::controller_client(info).await?;
    let resp = client
        .resize_volume(controller_proto::ResizeVolumeRequest {
            name: name.to_string(),
            size_bytes,
            grow_filesystem,
            allow_shrink,
        })
        .await?
        .into_inner();
    if !resp.success {
        bail!("{}", resp.message);
    }
    println!("{}", resp.message);
    if !resp.guest_status_note.is_empty() {
        println!("Note: {}", resp.guest_status_note);
    }
    if !resp.operation_id.is_empty() {
        println!("Operation: {}", resp.operation_id);
    }
    Ok(())
}

/// Copy a Ceph volume onto another CephCluster (`kctl volume copy`).
pub async fn copy(
    info: &ConnectionInfo,
    source: &str,
    dest: Option<&str>,
    to_cluster: &str,
    from_cluster: Option<&str>,
) -> Result<()> {
    copy_to_cluster(info, source, to_cluster, dest, from_cluster).await
}

/// Move a detached LVM or ZFS volume onto Ceph.
pub async fn convert_to_ceph(
    info: &ConnectionInfo,
    name: &str,
    to: &str,
    ceph_cluster: Option<&str>,
) -> Result<()> {
    if to != "ceph" {
        bail!("--to must be ceph (got '{to}')");
    }
    let mut client = client::controller_client(info).await?;
    let resp = client
        .convert_volume(controller_proto::ConvertVolumeRequest {
            volume: name.to_string(),
            target_storage_class: to.to_string(),
            ceph_cluster: ceph_cluster.unwrap_or_default().to_string(),
        })
        .await?
        .into_inner();
    if !resp.success {
        bail!("{}", resp.message);
    }
    println!("{}", resp.message);
    Ok(())
}

pub async fn encrypt(info: &ConnectionInfo, name: &str, destroy_data: bool) -> Result<()> {
    if !destroy_data {
        bail!(
            "in-place encryption formats LUKS on the RBD image and destroys existing data; \
             pass --destroy-data to acknowledge"
        );
    }
    let mut client = client::controller_client(info).await?;
    let resp = client
        .encrypt_volume(controller_proto::EncryptVolumeRequest {
            name: name.to_string(),
            destroy_data: true,
        })
        .await?
        .into_inner();
    if !resp.success {
        bail!("{}", resp.message);
    }
    println!("{}", resp.message);
    Ok(())
}

/// Backends `volume convert` accepts, checked before dialing the controller.
fn convert_target(to: &str) -> Result<&'static str> {
    match to.trim().to_ascii_lowercase().as_str() {
        "ceph" => Ok("ceph"),
        "lvm" | "zfs" => bail!(
            "converting to {} is not implemented; only --to ceph is",
            to.trim()
        ),
        "" => bail!("--to is required (ceph)"),
        other => bail!("unknown backend '{other}'; only ceph is supported"),
    }
}

/// Stop the VM, copy its LVM or ZFS disk onto Ceph, and switch it over.
pub async fn convert_vm(info: &ConnectionInfo, vm: &str, to: &str, yes: bool) -> Result<()> {
    let to = convert_target(to)?;
    let mut client =
        client::controller_client_with_timeout(info, client::LOCAL_DISK_MOVE_TIMEOUT).await?;
    let resp = client
        .convert_vm_storage(controller_proto::ConvertVmStorageRequest {
            vm: vm.to_string(),
            to: to.to_string(),
            allow_stop: yes,
        })
        .await?
        .into_inner();
    if !resp.success {
        bail!("{}", resp.message);
    }
    println!("{}", resp.message);
    if !resp.operation_id.is_empty() {
        println!("Operation: {}", resp.operation_id);
    }
    Ok(())
}

/// Copy a Ceph volume into a new volume on another CephCluster.
pub async fn copy_to_cluster(
    info: &ConnectionInfo,
    volume: &str,
    to_cluster: &str,
    new_name: Option<&str>,
    from_cluster: Option<&str>,
) -> Result<()> {
    if to_cluster.trim().is_empty() {
        bail!("--to-cluster is required");
    }
    let mut client =
        client::controller_client_with_timeout(info, client::LOCAL_DISK_MOVE_TIMEOUT).await?;
    let resp = client
        .copy_volume_to_cluster(controller_proto::CopyVolumeToClusterRequest {
            name: volume.to_string(),
            to_cluster: to_cluster.to_string(),
            new_name: new_name.unwrap_or("").to_string(),
            from_cluster: from_cluster.unwrap_or("").to_string(),
        })
        .await?
        .into_inner();
    if !resp.success {
        bail!("{}", resp.message);
    }
    println!("{}", resp.message);
    println!("Copied: {}", crate::client::format_bytes(resp.bytes_copied));
    Ok(())
}

pub async fn create_snapshot_policy(
    info: &ConnectionInfo,
    name: &str,
    vm: Option<String>,
    volume: Option<String>,
    schedule: String,
    keep: i32,
    enabled: bool,
) -> Result<()> {
    if keep <= 0 {
        bail!("--keep must be positive");
    }
    let mut client = client::controller_client(info).await?;
    let resp = client
        .create_snapshot_policy(controller_proto::CreateSnapshotPolicyRequest {
            policy: Some(controller_proto::SnapshotPolicy {
                name: name.to_string(),
                selector_vm: vm.unwrap_or_default(),
                selector_volume: volume.unwrap_or_default(),
                schedule,
                keep,
                enabled,
                last_run_at: String::new(),
                last_message: String::new(),
            }),
        })
        .await?
        .into_inner();
    if !resp.success {
        bail!("{}", resp.message);
    }
    println!("{}", resp.message);
    Ok(())
}

pub async fn list_snapshot_policies(info: &ConnectionInfo) -> Result<()> {
    let mut client = client::controller_client(info).await?;
    let resp = client
        .list_snapshot_policies(controller_proto::ListSnapshotPoliciesRequest {})
        .await?
        .into_inner();
    if resp.policies.is_empty() {
        println!("No snapshot policies.");
        return Ok(());
    }
    println!(
        "{:<20}  {:<12}  {:<14}  {:>4}  {:<8}  LAST",
        "NAME", "SCHEDULE", "SELECTOR", "KEEP", "ENABLED"
    );
    for p in resp.policies {
        let sel = if !p.selector_volume.is_empty() {
            format!("vol:{}", p.selector_volume)
        } else {
            format!("vm:{}", p.selector_vm)
        };
        println!(
            "{:<20}  {:<12}  {:<14}  {:>4}  {:<8}  {}",
            p.name,
            p.schedule,
            sel,
            p.keep,
            p.enabled,
            if p.last_message.is_empty() {
                p.last_run_at
            } else {
                p.last_message
            }
        );
    }
    Ok(())
}

pub async fn get_snapshot_policy(info: &ConnectionInfo, name: &str) -> Result<()> {
    let mut client = client::controller_client(info).await?;
    let resp = client
        .get_snapshot_policy(controller_proto::GetSnapshotPolicyRequest {
            name: name.to_string(),
        })
        .await?
        .into_inner();
    let Some(p) = resp.policy else {
        bail!("snapshot policy '{name}' not found");
    };
    println!("Name:           {}", p.name);
    println!("Selector VM:    {}", p.selector_vm);
    println!("Selector vol:   {}", p.selector_volume);
    println!("Schedule:       {}", p.schedule);
    println!("Keep:           {}", p.keep);
    println!("Enabled:        {}", p.enabled);
    println!("Last run:       {}", p.last_run_at);
    println!("Last message:   {}", p.last_message);
    Ok(())
}

pub async fn delete_snapshot_policy(info: &ConnectionInfo, name: &str) -> Result<()> {
    let mut client = client::controller_client(info).await?;
    let resp = client
        .delete_snapshot_policy(controller_proto::DeleteSnapshotPolicyRequest {
            name: name.to_string(),
        })
        .await?
        .into_inner();
    if !resp.success {
        bail!("{}", resp.message);
    }
    println!("{}", resp.message);
    Ok(())
}

pub async fn create(
    info: &ConnectionInfo,
    name: &str,
    size_bytes: i64,
    storage_class: &str,
    node: Option<&str>,
    volume_name: Option<&str>,
    vm: Option<String>,
    from_snapshot: Option<String>,
    encrypt: bool,
) -> Result<()> {
    if from_snapshot.as_ref().map(|s| s.is_empty()).unwrap_or(true) && size_bytes <= 0 {
        bail!("--size-bytes must be positive (unless --from-snapshot is set)");
    }
    let mut client = client::controller_client(info).await?;
    let resp = client
        .create_volume(controller_proto::CreateVolumeRequest {
            name: name.to_string(),
            size_bytes,
            storage_class: storage_class.to_string(),
            vm: vm.unwrap_or_default(),
            guest_format_json: String::new(),
            from_snapshot: from_snapshot.unwrap_or_default(),
            encrypt,
            node: node.unwrap_or("").to_string(),
            volume_name: volume_name.unwrap_or("").to_string(),
            rbd_namespace: String::new(),
            qos_iops: 0,
            qos_bps: 0,
        })
        .await?
        .into_inner();
    if !resp.success {
        bail!("{}", resp.message);
    }
    println!("{}", resp.message);
    Ok(())
}

pub async fn snapshot(info: &ConnectionInfo, volume: &str, name: Option<String>) -> Result<()> {
    let mut client = client::controller_client(info).await?;
    let resp = client
        .create_volume_snapshot(controller_proto::CreateVolumeSnapshotRequest {
            volume: volume.to_string(),
            name: name.unwrap_or_default(),
            consistency: "crash".into(),
        })
        .await?
        .into_inner();
    if !resp.success {
        bail!("{}", resp.message);
    }
    println!("{}", resp.message);
    Ok(())
}

pub async fn list_snapshots(info: &ConnectionInfo, volume: Option<String>) -> Result<()> {
    let mut client = client::controller_client(info).await?;
    let resp = client
        .list_volume_snapshots(controller_proto::ListVolumeSnapshotsRequest {
            volume: volume.unwrap_or_default(),
        })
        .await?
        .into_inner();
    if resp.snapshots.is_empty() {
        println!("No snapshots found.");
        return Ok(());
    }
    println!(
        "{:<24}  {:<18}  {:<12}  {:>10}  RBD_SNAP",
        "NAME", "VOLUME", "CONSISTENCY", "SIZE"
    );
    for s in resp.snapshots {
        println!(
            "{:<24}  {:<18}  {:<12}  {:>10}  {}",
            s.name,
            s.volume_name,
            s.consistency,
            crate::client::format_bytes(s.size_bytes),
            s.rbd_snap
        );
    }
    Ok(())
}

pub async fn delete_snapshot(info: &ConnectionInfo, name: &str) -> Result<()> {
    let mut client = client::controller_client(info).await?;
    let resp = client
        .delete_volume_snapshot(controller_proto::DeleteVolumeSnapshotRequest {
            name: name.to_string(),
        })
        .await?
        .into_inner();
    if !resp.success {
        bail!("{}", resp.message);
    }
    println!("{}", resp.message);
    Ok(())
}

pub async fn restore(info: &ConnectionInfo, volume: &str, snapshot: &str) -> Result<()> {
    let mut client = client::controller_client(info).await?;
    let resp = client
        .restore_volume(controller_proto::RestoreVolumeRequest {
            volume: volume.to_string(),
            snapshot: snapshot.to_string(),
        })
        .await?
        .into_inner();
    if !resp.success {
        bail!("{}", resp.message);
    }
    println!("{}", resp.message);
    Ok(())
}

pub async fn flatten(info: &ConnectionInfo, name: &str) -> Result<()> {
    let mut client = client::controller_client(info).await?;
    let resp = client
        .flatten_volume(controller_proto::FlattenVolumeRequest {
            name: name.to_string(),
        })
        .await?
        .into_inner();
    if !resp.success {
        bail!("{}", resp.message);
    }
    println!("{}", resp.message);
    Ok(())
}

pub async fn attach(info: &ConnectionInfo, name: &str, vm: &str) -> Result<()> {
    let mut client = client::controller_client(info).await?;
    let resp = client
        .attach_volume(controller_proto::AttachVolumeRequest {
            name: name.to_string(),
            vm: vm.to_string(),
        })
        .await?
        .into_inner();
    if !resp.success {
        bail!("{}", resp.message);
    }
    println!("{}", resp.message);
    Ok(())
}

pub async fn detach(info: &ConnectionInfo, name: &str) -> Result<()> {
    let mut client = client::controller_client(info).await?;
    let resp = client
        .detach_volume(controller_proto::DetachVolumeRequest {
            name: name.to_string(),
        })
        .await?
        .into_inner();
    if !resp.success {
        bail!("{}", resp.message);
    }
    println!("{}", resp.message);
    Ok(())
}

pub async fn delete(info: &ConnectionInfo, name: &str) -> Result<()> {
    let mut client = client::controller_client(info).await?;
    let resp = client
        .delete_volume(controller_proto::DeleteVolumeRequest {
            name: name.to_string(),
        })
        .await?
        .into_inner();
    if !resp.success {
        bail!("{}", resp.message);
    }
    println!("{}", resp.message);
    Ok(())
}

pub async fn format(info: &ConnectionInfo, name: &str, fs: &str, force: bool) -> Result<()> {
    let mut client = client::controller_client(info).await?;
    let resp = client
        .format_volume(controller_proto::FormatVolumeRequest {
            name: name.to_string(),
            fs_type: fs.to_string(),
            force,
        })
        .await?
        .into_inner();
    if !resp.success {
        bail!("{}", resp.message);
    }
    println!("{}", resp.message);
    Ok(())
}

pub async fn mount(
    info: &ConnectionInfo,
    name: &str,
    path: &str,
    fs: Option<&str>,
    persistent: bool,
) -> Result<()> {
    let mut client = client::controller_client(info).await?;
    let resp = client
        .mount_volume(controller_proto::MountVolumeRequest {
            name: name.to_string(),
            mount_point: path.to_string(),
            fs_type: fs.unwrap_or("").to_string(),
            persistent,
        })
        .await?
        .into_inner();
    if !resp.success {
        bail!("{}", resp.message);
    }
    println!("{}", resp.message);
    Ok(())
}

pub async fn unmount(info: &ConnectionInfo, name: &str, persistent: bool) -> Result<()> {
    let mut client = client::controller_client(info).await?;
    let resp = client
        .unmount_volume(controller_proto::UnmountVolumeRequest {
            name: name.to_string(),
            persistent,
        })
        .await?
        .into_inner();
    if !resp.success {
        bail!("{}", resp.message);
    }
    println!("{}", resp.message);
    Ok(())
}

/// Export a volume snapshot to a sparse raw file.
pub async fn export(
    info: &ConnectionInfo,
    volume: &str,
    snapshot: &str,
    from_snapshot: Option<&str>,
    output: &Path,
    block_size: i32,
) -> Result<()> {
    let mut client = client::controller_client(info).await?;
    let extents = if let Some(from) = from_snapshot {
        let resp = client
            .list_volume_changed_extents(controller_proto::ListVolumeChangedExtentsRequest {
                volume_name: volume.to_string(),
                from_snapshot: from.to_string(),
                to_snapshot: snapshot.to_string(),
            })
            .await?
            .into_inner();
        if !resp.supported {
            eprintln!(
                "warning: changed-extent listing not supported ({}); reading whole snapshot",
                resp.source
            );
            Vec::new()
        } else {
            resp.extents
        }
    } else {
        Vec::new()
    };

    let exposed = client
        .expose_volume_snapshot(controller_proto::ExposeVolumeSnapshotRequest {
            volume_name: volume.to_string(),
            snapshot_name: snapshot.to_string(),
            lease_seconds: 3600,
        })
        .await?
        .into_inner();

    let result = export_blocks(
        &mut client,
        &exposed.lease_id,
        &extents,
        block_size,
        exposed.size_bytes,
        output,
    )
    .await;

    let _ = client
        .unexpose_volume_snapshot(controller_proto::UnexposeVolumeSnapshotRequest {
            lease_id: exposed.lease_id,
        })
        .await;

    result?;
    println!(
        "exported volume '{volume}' snapshot '{snapshot}' -> {} ({} logical)",
        output.display(),
        crate::client::format_bytes(exposed.size_bytes)
    );
    Ok(())
}

async fn export_blocks(
    client: &mut controller_proto::controller_client::ControllerClient<tonic::transport::Channel>,
    lease_id: &str,
    extents: &[controller_proto::VolumeExtent],
    block_size: i32,
    size_bytes: i64,
    output: &Path,
) -> Result<()> {
    let mut file =
        std::fs::File::create(output).with_context(|| format!("create {}", output.display()))?;
    if size_bytes > 0 {
        file.set_len(size_bytes as u64)
            .with_context(|| format!("truncate {}", output.display()))?;
    }

    let mut stream = client
        .read_volume_blocks(controller_proto::ReadVolumeBlocksRequest {
            lease_id: lease_id.to_string(),
            extents: extents.to_vec(),
            block_size,
            elide_zero_blocks: true,
        })
        .await?
        .into_inner();

    let mut bytes_written = 0i64;
    let mut zero_runs = 0i64;
    while let Some(item) = stream.next().await {
        let block = item?;
        match block.body {
            Some(controller_proto::volume_block::Body::Data(data)) => {
                file.seek(SeekFrom::Start(block.offset as u64))?;
                file.write_all(&data)?;
                bytes_written += data.len() as i64;
            }
            Some(controller_proto::volume_block::Body::ZeroLength(len)) => {
                // Sparse: leave a hole (file already sized).
                zero_runs += 1;
                let _ = len;
            }
            None => {}
        }
    }
    file.sync_all()?;
    eprintln!("wrote {bytes_written} bytes, {zero_runs} zero-runs elided");
    Ok(())
}

/// Import a sparse raw file into a new or existing volume.
#[allow(clippy::too_many_arguments)]
pub async fn import(
    info: &ConnectionInfo,
    file: &Path,
    name: Option<&str>,
    base: Option<&str>,
    size_bytes: Option<i64>,
    storage_class: &str,
    node: Option<&str>,
    punch_zeros: bool,
) -> Result<()> {
    let meta = std::fs::metadata(file).with_context(|| format!("stat {}", file.display()))?;
    let logical_size = size_bytes.unwrap_or(meta.len() as i64);
    if logical_size <= 0 {
        bail!("import size must be positive");
    }

    let mut client = client::controller_client(info).await?;
    let volume_name = if let Some(base) = base {
        base.to_string()
    } else {
        let name =
            name.ok_or_else(|| anyhow::anyhow!("--name is required unless --base is set"))?;
        let resp = client
            .create_volume(controller_proto::CreateVolumeRequest {
                name: name.to_string(),
                size_bytes: logical_size,
                storage_class: storage_class.to_string(),
                vm: String::new(),
                guest_format_json: String::new(),
                from_snapshot: String::new(),
                encrypt: false,
                node: node.unwrap_or("").to_string(),
                volume_name: String::new(),
                rbd_namespace: String::new(),
                qos_iops: 0,
                qos_bps: 0,
            })
            .await?
            .into_inner();
        if !resp.success {
            bail!("{}", resp.message);
        }
        name.to_string()
    };

    let data = std::fs::read(file).with_context(|| format!("read {}", file.display()))?;
    let block_size = 4 * 1024 * 1024i64;
    let (tx, rx) = tokio::sync::mpsc::channel(4);
    let volume_name_for_stream = volume_name.clone();
    tokio::spawn(async move {
        let _ = tx
            .send(controller_proto::WriteVolumeBlocksRequest {
                msg: Some(controller_proto::write_volume_blocks_request::Msg::Target(
                    controller_proto::WriteVolumeBlocksTarget {
                        volume_name: volume_name_for_stream,
                        punch_zero_runs: punch_zeros,
                    },
                )),
            })
            .await;
        let mut offset = 0i64;
        while offset < data.len() as i64 {
            let end = ((offset + block_size) as usize).min(data.len());
            let chunk = &data[offset as usize..end];
            let body = if chunk.iter().all(|&b| b == 0) {
                controller_proto::volume_block::Body::ZeroLength(chunk.len() as i64)
            } else {
                controller_proto::volume_block::Body::Data(chunk.to_vec())
            };
            if tx
                .send(controller_proto::WriteVolumeBlocksRequest {
                    msg: Some(controller_proto::write_volume_blocks_request::Msg::Block(
                        controller_proto::VolumeBlock {
                            offset,
                            body: Some(body),
                        },
                    )),
                })
                .await
                .is_err()
            {
                break;
            }
            offset = end as i64;
        }
    });
    let outbound = tokio_stream::wrappers::ReceiverStream::new(rx);
    let resp = client.write_volume_blocks(outbound).await?.into_inner();
    println!(
        "imported {} into volume '{volume_name}' ({} bytes, {} blocks, {} zero-runs)",
        file.display(),
        resp.bytes_written,
        resp.blocks_written,
        resp.zero_runs
    );
    Ok(())
}

pub async fn trash_list(
    info: &ConnectionInfo,
    pool: Option<String>,
    namespace: Option<String>,
) -> Result<()> {
    let mut client = client::controller_client(info).await?;
    let resp = client
        .list_volume_trash(controller_proto::ListVolumeTrashRequest {
            pool: pool.unwrap_or_default(),
            namespace: namespace.unwrap_or_default(),
        })
        .await
        .context("list_volume_trash rpc")?
        .into_inner();
    if resp.entries.is_empty() {
        println!("No trash entries.");
        return Ok(());
    }
    for e in resp.entries {
        let ns = if e.namespace.is_empty() {
            String::new()
        } else {
            format!(" ns={}", e.namespace)
        };
        println!("{}\t{}/{}\t{}{}", e.id, e.pool, e.name, e.deletion_time, ns);
    }
    Ok(())
}

pub async fn trash_purge(
    info: &ConnectionInfo,
    yes: bool,
    pool: Option<String>,
    trash_id: Option<String>,
    namespace: Option<String>,
) -> Result<()> {
    if !yes {
        bail!("pass --yes to permanently purge RBD trash");
    }
    let mut client = client::controller_client(info).await?;
    let resp = client
        .purge_volume_trash(controller_proto::PurgeVolumeTrashRequest {
            pool: pool.unwrap_or_default(),
            namespace: namespace.unwrap_or_default(),
            trash_id: trash_id.unwrap_or_default(),
            confirm: true,
        })
        .await
        .context("purge_volume_trash rpc")?
        .into_inner();
    if !resp.success {
        bail!("{}", resp.message);
    }
    println!("{}", resp.message);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::convert_target;

    #[test]
    fn convert_accepts_only_ceph() {
        assert_eq!(convert_target(" Ceph ").unwrap(), "ceph");
        assert!(convert_target("lvm")
            .unwrap_err()
            .to_string()
            .contains("not implemented"));
        assert!(convert_target("zfs").is_err());
        assert!(convert_target("").is_err());
        assert!(convert_target("nfs")
            .unwrap_err()
            .to_string()
            .contains("unknown"));
    }
}
