use crate::client::{self, controller_proto};
use crate::config::ConnectionInfo;
use crate::output;
use anyhow::{bail, Result};

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
        "{:<20}  {:<12}  {:<14}  {:>4}  {:<8}  {}",
        "NAME", "SCHEDULE", "SELECTOR", "KEEP", "ENABLED", "LAST"
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
            storage_class: "ceph".into(),
            vm: vm.unwrap_or_default(),
            guest_format_json: String::new(),
            from_snapshot: from_snapshot.unwrap_or_default(),
            encrypt,
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
        "{:<24}  {:<18}  {:<12}  {:>10}  {}",
        "NAME", "VOLUME", "CONSISTENCY", "SIZE", "RBD_SNAP"
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
