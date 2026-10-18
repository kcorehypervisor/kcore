use std::path::Path;
use std::pin::Pin;
use std::sync::Arc;

use tokio_stream::wrappers::ReceiverStream;
use tonic::{Request, Response, Status};

use crate::auth::{self, CN_CONTROLLER_PREFIX, CN_KCTL};
use crate::lvm_ops;
use crate::proto;
use crate::storage::{self, export, StorageAdapter};
use crate::zfs_ops;

pub struct StorageService {
    storage: Arc<dyn StorageAdapter>,
    lvm_vg: Option<String>,
    zfs_pool: Option<String>,
}

impl StorageService {
    pub fn new() -> Self {
        Self {
            storage: storage::default_adapter(),
            lvm_vg: None,
            zfs_pool: None,
        }
    }

    pub fn new_with_storage(storage: Arc<dyn StorageAdapter>) -> Self {
        Self {
            storage,
            lvm_vg: None,
            zfs_pool: None,
        }
    }

    pub fn with_lvm_vg(mut self, vg: impl Into<String>) -> Self {
        self.lvm_vg = Some(vg.into());
        self
    }

    pub fn with_zfs_pool(mut self, pool: impl Into<String>) -> Self {
        self.zfs_pool = Some(pool.into());
        self
    }

    fn host_fs_ready(&self) -> Result<(), Status> {
        if self.lvm_vg.is_some() || self.zfs_pool.is_some() {
            Ok(())
        } else {
            Err(Status::failed_precondition(
                "node storage backend is not lvm or zfs",
            ))
        }
    }
}

#[tonic::async_trait]
impl proto::node_storage_server::NodeStorage for StorageService {
    async fn create_volume(
        &self,
        request: Request<proto::CreateVolumeRequest>,
    ) -> Result<Response<proto::CreateVolumeResponse>, Status> {
        auth::require_peer(&request, &[CN_CONTROLLER_PREFIX, CN_KCTL])?;
        let req = request.into_inner();
        let storage = Arc::clone(&self.storage);
        let resp = tokio::task::spawn_blocking(move || {
            storage
                .create_volume(storage::CreateVolumeRequest {
                    volume_id: req.volume_id,
                    storage_class: req.storage_class,
                    size_bytes: req.size_bytes,
                    parameters: req.parameters,
                    lv_name: req.lv_name,
                })
                .map(|backend_handle| proto::CreateVolumeResponse { backend_handle })
        })
        .await
        .map_err(|e| Status::internal(format!("task join: {e}")))??;
        Ok(Response::new(resp))
    }

    async fn delete_volume(
        &self,
        request: Request<proto::DeleteVolumeRequest>,
    ) -> Result<Response<proto::DeleteVolumeResponse>, Status> {
        auth::require_peer(&request, &[CN_CONTROLLER_PREFIX, CN_KCTL])?;
        let req = request.into_inner();
        let storage = Arc::clone(&self.storage);
        tokio::task::spawn_blocking(move || storage.delete_volume(&req.backend_handle))
            .await
            .map_err(|e| Status::internal(format!("task join: {e}")))??;
        Ok(Response::new(proto::DeleteVolumeResponse {}))
    }

    async fn attach_volume(
        &self,
        request: Request<proto::AttachVolumeRequest>,
    ) -> Result<Response<proto::AttachVolumeResponse>, Status> {
        auth::require_peer(&request, &[CN_CONTROLLER_PREFIX, CN_KCTL])?;
        let req = request.into_inner();
        let storage = Arc::clone(&self.storage);
        tokio::task::spawn_blocking(move || {
            storage.attach_volume(storage::AttachVolumeRequest {
                backend_handle: req.backend_handle,
                vm_id: req.vm_id,
                target_device: req.target_device,
                bus: req.bus,
            })
        })
        .await
        .map_err(|e| Status::internal(format!("task join: {e}")))??;
        Ok(Response::new(proto::AttachVolumeResponse {}))
    }

    async fn detach_volume(
        &self,
        request: Request<proto::DetachVolumeRequest>,
    ) -> Result<Response<proto::DetachVolumeResponse>, Status> {
        auth::require_peer(&request, &[CN_CONTROLLER_PREFIX, CN_KCTL])?;
        let req = request.into_inner();
        let storage = Arc::clone(&self.storage);
        tokio::task::spawn_blocking(move || {
            storage.detach_volume(storage::DetachVolumeRequest {
                backend_handle: req.backend_handle,
                vm_id: req.vm_id,
            })
        })
        .await
        .map_err(|e| Status::internal(format!("task join: {e}")))??;
        Ok(Response::new(proto::DetachVolumeResponse {}))
    }

    async fn snapshot_volume(
        &self,
        request: Request<proto::SnapshotVolumeRequest>,
    ) -> Result<Response<proto::SnapshotVolumeResponse>, Status> {
        auth::require_peer(&request, &[CN_CONTROLLER_PREFIX, CN_KCTL])?;
        let req = request.into_inner();
        let resp = tokio::task::spawn_blocking(move || {
            export::snapshot_volume(
                &req.storage_class,
                &req.backend_handle,
                &req.snapshot_name,
                req.protect,
            )
            .map(|snapshot_handle| proto::SnapshotVolumeResponse { snapshot_handle })
        })
        .await
        .map_err(|e| Status::internal(format!("task join: {e}")))??;
        Ok(Response::new(resp))
    }

    async fn delete_volume_snapshot(
        &self,
        request: Request<proto::DeleteVolumeSnapshotRequest>,
    ) -> Result<Response<proto::DeleteVolumeSnapshotResponse>, Status> {
        auth::require_peer(&request, &[CN_CONTROLLER_PREFIX, CN_KCTL])?;
        let req = request.into_inner();
        tokio::task::spawn_blocking(move || {
            export::delete_volume_snapshot(
                &req.storage_class,
                &req.backend_handle,
                &req.snapshot_name,
                req.unprotect,
            )
        })
        .await
        .map_err(|e| Status::internal(format!("task join: {e}")))??;
        Ok(Response::new(proto::DeleteVolumeSnapshotResponse {}))
    }

    async fn clone_volume(
        &self,
        request: Request<proto::CloneVolumeRequest>,
    ) -> Result<Response<proto::CloneVolumeResponse>, Status> {
        auth::require_peer(&request, &[CN_CONTROLLER_PREFIX, CN_KCTL])?;
        let req = request.into_inner();
        let resp = tokio::task::spawn_blocking(move || {
            export::clone_volume(
                &req.storage_class,
                &req.parent_handle,
                &req.parent_snapshot,
                &req.child_image,
            )
            .map(|backend_handle| proto::CloneVolumeResponse { backend_handle })
        })
        .await
        .map_err(|e| Status::internal(format!("task join: {e}")))??;
        Ok(Response::new(resp))
    }

    async fn fence_volume(
        &self,
        request: Request<proto::FenceVolumeRequest>,
    ) -> Result<Response<proto::FenceVolumeResponse>, Status> {
        auth::require_peer(&request, &[CN_CONTROLLER_PREFIX, CN_KCTL])?;
        let req = request.into_inner();
        let blocklisted = tokio::task::spawn_blocking(move || {
            storage::ceph_fence_volume(&req.backend_handle, &req.source_hosts)
        })
        .await
        .map_err(|e| Status::internal(format!("task join: {e}")))??;
        Ok(Response::new(proto::FenceVolumeResponse { blocklisted }))
    }

    async fn rollback_volume(
        &self,
        request: Request<proto::RollbackVolumeRequest>,
    ) -> Result<Response<proto::RollbackVolumeResponse>, Status> {
        auth::require_peer(&request, &[CN_CONTROLLER_PREFIX, CN_KCTL])?;
        let req = request.into_inner();
        tokio::task::spawn_blocking(move || {
            export::rollback_volume(&req.storage_class, &req.backend_handle, &req.snapshot_name)
        })
        .await
        .map_err(|e| Status::internal(format!("task join: {e}")))??;
        Ok(Response::new(proto::RollbackVolumeResponse {}))
    }

    async fn expose_snapshot(
        &self,
        request: Request<proto::ExposeSnapshotRequest>,
    ) -> Result<Response<proto::ExposeSnapshotResponse>, Status> {
        auth::require_peer(&request, &[CN_CONTROLLER_PREFIX, CN_KCTL])?;
        let req = request.into_inner();
        let leases = export::global_lease_table();
        let lease = tokio::task::spawn_blocking(move || {
            export::expose_snapshot(
                &req.storage_class,
                &req.backend_handle,
                &req.snapshot_name,
                req.lease_seconds,
                &leases,
            )
        })
        .await
        .map_err(|e| Status::internal(format!("task join: {e}")))??;
        Ok(Response::new(proto::ExposeSnapshotResponse {
            lease_id: lease.lease_id,
            device_path: lease.device_path,
            size_bytes: lease.size_bytes,
            block_size: lease.block_size,
        }))
    }

    async fn unexpose_snapshot(
        &self,
        request: Request<proto::UnexposeSnapshotRequest>,
    ) -> Result<Response<proto::UnexposeSnapshotResponse>, Status> {
        auth::require_peer(&request, &[CN_CONTROLLER_PREFIX, CN_KCTL])?;
        let req = request.into_inner();
        let leases = export::global_lease_table();
        tokio::task::spawn_blocking(move || export::unexpose_snapshot(&req.lease_id, &leases))
            .await
            .map_err(|e| Status::internal(format!("task join: {e}")))??;
        Ok(Response::new(proto::UnexposeSnapshotResponse {}))
    }

    async fn list_changed_extents(
        &self,
        request: Request<proto::ListChangedExtentsRequest>,
    ) -> Result<Response<proto::ListChangedExtentsResponse>, Status> {
        auth::require_peer(&request, &[CN_CONTROLLER_PREFIX, CN_KCTL])?;
        let req = request.into_inner();
        let (extents, supported, source) = tokio::task::spawn_blocking(move || {
            export::list_changed_extents(
                &req.storage_class,
                &req.backend_handle,
                &req.from_snapshot,
                &req.to_snapshot,
            )
        })
        .await
        .map_err(|e| Status::internal(format!("task join: {e}")))??;
        Ok(Response::new(proto::ListChangedExtentsResponse {
            extents: extents
                .into_iter()
                .map(|e| proto::Extent {
                    offset: e.offset,
                    length: e.length,
                })
                .collect(),
            supported,
            source,
        }))
    }

    type ReadVolumeExtentsStream =
        Pin<Box<dyn tokio_stream::Stream<Item = Result<proto::VolumeBlock, Status>> + Send>>;

    async fn read_volume_extents(
        &self,
        request: Request<proto::ReadVolumeExtentsRequest>,
    ) -> Result<Response<Self::ReadVolumeExtentsStream>, Status> {
        auth::require_peer(&request, &[CN_CONTROLLER_PREFIX, CN_KCTL])?;
        let req = request.into_inner();
        let leases = export::global_lease_table();
        let lease = leases.get(&req.lease_id)?;
        let extents: Vec<export::Extent> = req
            .extents
            .iter()
            .map(|e| export::Extent {
                offset: e.offset,
                length: e.length,
            })
            .collect();
        let block_size = if req.block_size > 0 {
            req.block_size as i64
        } else {
            export::DEFAULT_BLOCK_SIZE
        };
        let elide = req.elide_zero_blocks;
        let device = lease.device_path.clone();
        let (tx, rx) = tokio::sync::mpsc::channel(8);
        tokio::task::spawn_blocking(move || {
            let units =
                match export::read_extent_units(Path::new(&device), &extents, block_size, elide) {
                    Ok(u) => u,
                    Err(e) => {
                        let _ = tx.blocking_send(Err(Status::from(e)));
                        return;
                    }
                };
            for (unit, data) in units {
                let block = match unit {
                    export::TransferUnit::Data { offset, .. } => proto::VolumeBlock {
                        offset,
                        body: Some(proto::volume_block::Body::Data(data.unwrap_or_default())),
                    },
                    export::TransferUnit::ZeroRun { offset, length } => proto::VolumeBlock {
                        offset,
                        body: Some(proto::volume_block::Body::ZeroLength(length)),
                    },
                };
                if tx.blocking_send(Ok(block)).is_err() {
                    break;
                }
            }
        });
        let stream: Self::ReadVolumeExtentsStream = Box::pin(ReceiverStream::new(rx));
        Ok(Response::new(stream))
    }

    async fn write_volume_extents(
        &self,
        request: Request<tonic::Streaming<proto::WriteVolumeExtentsRequest>>,
    ) -> Result<Response<proto::WriteVolumeExtentsResponse>, Status> {
        auth::require_peer(&request, &[CN_CONTROLLER_PREFIX, CN_KCTL])?;
        let mut stream = request.into_inner();
        let first = stream
            .message()
            .await?
            .ok_or_else(|| Status::invalid_argument("write stream is empty"))?;
        let target = match first.msg {
            Some(proto::write_volume_extents_request::Msg::Target(t)) => t,
            _ => {
                return Err(Status::invalid_argument(
                    "first write message must set target",
                ));
            }
        };
        let device = resolve_write_device(&target.backend_handle, &target.storage_class)?;
        let punch = target.punch_zero_runs;
        let mut bytes_written = 0i64;
        let mut blocks_written = 0i64;
        let mut zero_runs = 0i64;
        while let Some(msg) = stream.message().await? {
            let block = match msg.msg {
                Some(proto::write_volume_extents_request::Msg::Block(b)) => b,
                Some(proto::write_volume_extents_request::Msg::Target(_)) => {
                    return Err(Status::invalid_argument(
                        "target may only appear as the first message",
                    ));
                }
                None => continue,
            };
            let (unit, data) = match block.body {
                Some(proto::volume_block::Body::Data(d)) => (
                    export::TransferUnit::Data {
                        offset: block.offset,
                        length: d.len() as i64,
                    },
                    Some(d),
                ),
                Some(proto::volume_block::Body::ZeroLength(len)) => (
                    export::TransferUnit::ZeroRun {
                        offset: block.offset,
                        length: len,
                    },
                    None,
                ),
                None => {
                    return Err(Status::invalid_argument("block missing body"));
                }
            };
            let device_path = device.clone();
            let (b, bl, z) = tokio::task::spawn_blocking(move || {
                export::write_extent_unit(Path::new(&device_path), &unit, data.as_deref(), punch)
            })
            .await
            .map_err(|e| Status::internal(format!("task join: {e}")))??;
            bytes_written += b;
            blocks_written += bl;
            zero_runs += z;
        }
        Ok(Response::new(proto::WriteVolumeExtentsResponse {
            bytes_written,
            blocks_written,
            zero_runs,
        }))
    }

    async fn flatten_volume(
        &self,
        request: Request<proto::FlattenVolumeRequest>,
    ) -> Result<Response<proto::FlattenVolumeResponse>, Status> {
        auth::require_peer(&request, &[CN_CONTROLLER_PREFIX, CN_KCTL])?;
        let req = request.into_inner();
        tokio::task::spawn_blocking(move || storage::ceph_flatten_volume(&req.backend_handle))
            .await
            .map_err(|e| Status::internal(format!("task join: {e}")))??;
        Ok(Response::new(proto::FlattenVolumeResponse {}))
    }

    async fn resize_volume(
        &self,
        request: Request<proto::ResizeVolumeRequest>,
    ) -> Result<Response<proto::ResizeVolumeResponse>, Status> {
        auth::require_peer(&request, &[CN_CONTROLLER_PREFIX, CN_KCTL])?;
        let req = request.into_inner();
        let allow_shrink = req.allow_shrink;
        let vg = self.lvm_vg.clone();
        let pool = self.zfs_pool.clone();
        let (size, message) = tokio::task::spawn_blocking(move || {
            let handle = req.backend_handle.trim();
            if handle.starts_with("/dev/zvol/") {
                let pool = pool.ok_or_else(|| {
                    storage::StorageError::new(
                        storage::ErrorKind::FailedPrecondition,
                        "node storage backend is not zfs",
                    )
                })?;
                let message = zfs_ops::resize_volume(&pool, handle, req.size_bytes, allow_shrink)
                    .map_err(lvm_to_storage)?;
                Ok::<_, storage::StorageError>((req.size_bytes, message))
            } else if handle.starts_with("/dev/") {
                let vg = vg.ok_or_else(|| {
                    storage::StorageError::new(
                        storage::ErrorKind::FailedPrecondition,
                        "node storage backend is not lvm",
                    )
                })?;
                let message = lvm_ops::resize_volume(&vg, handle, req.size_bytes, allow_shrink)
                    .map_err(lvm_to_storage)?;
                Ok::<_, storage::StorageError>((req.size_bytes, message))
            } else {
                let size =
                    storage::ceph_resize_volume(&req.backend_handle, req.size_bytes, allow_shrink)?;
                Ok((size, format!("resized to {size} bytes")))
            }
        })
        .await
        .map_err(|e| Status::internal(format!("task join: {e}")))??;
        Ok(Response::new(proto::ResizeVolumeResponse {
            success: true,
            message,
            size_bytes: size,
        }))
    }

    async fn format_volume(
        &self,
        request: Request<proto::FormatVolumeRequest>,
    ) -> Result<Response<proto::FormatVolumeResponse>, Status> {
        auth::require_peer(&request, &[CN_CONTROLLER_PREFIX, CN_KCTL])?;
        let vg = self.lvm_vg.clone();
        let pool = self.zfs_pool.clone();
        let req = request.into_inner();
        let message = tokio::task::spawn_blocking(move || {
            if req.backend_handle.trim().starts_with("/dev/zvol/") {
                let pool = pool.ok_or_else(|| {
                    storage::StorageError::new(
                        storage::ErrorKind::FailedPrecondition,
                        "node storage backend is not zfs",
                    )
                })?;
                zfs_ops::format_volume(&pool, &req.backend_handle, &req.fs_type, req.force)
                    .map_err(lvm_to_storage)
            } else {
                let vg = vg.ok_or_else(|| {
                    storage::StorageError::new(
                        storage::ErrorKind::FailedPrecondition,
                        "node storage backend is not lvm",
                    )
                })?;
                lvm_ops::format_volume(&vg, &req.backend_handle, &req.fs_type, req.force)
                    .map_err(lvm_to_storage)
            }
        })
        .await
        .map_err(|e| Status::internal(format!("task join: {e}")))??;
        Ok(Response::new(proto::FormatVolumeResponse {
            success: true,
            message,
        }))
    }

    async fn mount_volume(
        &self,
        request: Request<proto::MountVolumeRequest>,
    ) -> Result<Response<proto::MountVolumeResponse>, Status> {
        auth::require_peer(&request, &[CN_CONTROLLER_PREFIX, CN_KCTL])?;
        let vg = self.lvm_vg.clone();
        let pool = self.zfs_pool.clone();
        let req = request.into_inner();
        let mount_point = req.mount_point.clone();
        let message = tokio::task::spawn_blocking(move || {
            if req.backend_handle.trim().starts_with("/dev/zvol/") {
                let pool = pool.ok_or_else(|| {
                    storage::StorageError::new(
                        storage::ErrorKind::FailedPrecondition,
                        "node storage backend is not zfs",
                    )
                })?;
                zfs_ops::mount_volume(
                    &pool,
                    &req.backend_handle,
                    &req.mount_point,
                    &req.fs_type,
                    req.persistent,
                )
                .map_err(lvm_to_storage)
            } else {
                let vg = vg.ok_or_else(|| {
                    storage::StorageError::new(
                        storage::ErrorKind::FailedPrecondition,
                        "node storage backend is not lvm",
                    )
                })?;
                lvm_ops::mount_volume(
                    &vg,
                    &req.backend_handle,
                    &req.mount_point,
                    &req.fs_type,
                    req.persistent,
                )
                .map_err(lvm_to_storage)
            }
        })
        .await
        .map_err(|e| Status::internal(format!("task join: {e}")))??;
        Ok(Response::new(proto::MountVolumeResponse {
            success: true,
            message,
            mount_point,
        }))
    }

    async fn unmount_volume(
        &self,
        request: Request<proto::UnmountVolumeRequest>,
    ) -> Result<Response<proto::UnmountVolumeResponse>, Status> {
        auth::require_peer(&request, &[CN_CONTROLLER_PREFIX, CN_KCTL])?;
        self.host_fs_ready()?;
        let req = request.into_inner();
        let message = tokio::task::spawn_blocking(move || {
            lvm_ops::unmount_volume(&req.mount_point, req.persistent).map_err(lvm_to_storage)
        })
        .await
        .map_err(|e| Status::internal(format!("task join: {e}")))??;
        Ok(Response::new(proto::UnmountVolumeResponse {
            success: true,
            message,
        }))
    }

    async fn format_encrypted_volume(
        &self,
        request: Request<proto::FormatEncryptedVolumeRequest>,
    ) -> Result<Response<proto::FormatEncryptedVolumeResponse>, Status> {
        auth::require_peer(&request, &[CN_CONTROLLER_PREFIX])?;
        let req = request.into_inner();
        let mapper = tokio::task::spawn_blocking(move || {
            let rbd = resolve_rbd_device(&req.rbd_device)?;
            crate::volume_crypto::format_and_open(&rbd, &req.dek, &req.mapper_name)
        })
        .await
        .map_err(|e| Status::internal(format!("task join: {e}")))?
        .map_err(Status::internal)?;
        Ok(Response::new(proto::FormatEncryptedVolumeResponse {
            success: true,
            message: format!("formatted and opened {mapper}"),
            mapper_path: mapper,
        }))
    }

    async fn unlock_encrypted_volume(
        &self,
        request: Request<proto::UnlockEncryptedVolumeRequest>,
    ) -> Result<Response<proto::UnlockEncryptedVolumeResponse>, Status> {
        auth::require_peer(&request, &[CN_CONTROLLER_PREFIX])?;
        let req = request.into_inner();
        let mapper = tokio::task::spawn_blocking(move || {
            let rbd = resolve_rbd_device(&req.rbd_device)?;
            if req.resize {
                crate::volume_crypto::resize(&req.mapper_name, &req.dek)?;
            }
            crate::volume_crypto::open(&rbd, &req.dek, &req.mapper_name)
        })
        .await
        .map_err(|e| Status::internal(format!("task join: {e}")))?
        .map_err(Status::internal)?;
        Ok(Response::new(proto::UnlockEncryptedVolumeResponse {
            success: true,
            message: format!("opened {mapper}"),
            mapper_path: mapper,
        }))
    }

    async fn lock_encrypted_volume(
        &self,
        request: Request<proto::LockEncryptedVolumeRequest>,
    ) -> Result<Response<proto::LockEncryptedVolumeResponse>, Status> {
        auth::require_peer(&request, &[CN_CONTROLLER_PREFIX])?;
        let req = request.into_inner();
        tokio::task::spawn_blocking(move || crate::volume_crypto::close(&req.mapper_name))
            .await
            .map_err(|e| Status::internal(format!("task join: {e}")))?
            .map_err(Status::internal)?;
        Ok(Response::new(proto::LockEncryptedVolumeResponse {
            success: true,
            message: "closed".into(),
        }))
    }

    async fn prepare_instant_disk(
        &self,
        request: Request<proto::PrepareInstantDiskRequest>,
    ) -> Result<Response<proto::PrepareInstantDiskResponse>, Status> {
        auth::require_peer(&request, &[CN_CONTROLLER_PREFIX, CN_KCTL])?;
        let req = request.into_inner();
        let vm_name = sanitize_instant_name(&req.vm_name);
        let dir = std::path::PathBuf::from(format!("/var/lib/kcore/instant/{vm_name}"));
        let overlay =
            crate::instant::prepare_overlay(&req.nbd_url, &dir).map_err(Status::internal)?;
        let memory = req.memory_bytes.max(0) as u64;
        let vcpus = req.vcpus.max(0) as u32;
        match crate::instant::boot_overlay(&vm_name, &overlay, memory, vcpus).await {
            Ok(pid) => Ok(Response::new(proto::PrepareInstantDiskResponse {
                success: true,
                message: format!("pid {pid}"),
                disk_path: overlay.display().to_string(),
            })),
            Err(e) => Ok(Response::new(proto::PrepareInstantDiskResponse {
                success: false,
                message: e,
                disk_path: overlay.display().to_string(),
            })),
        }
    }

    async fn finalize_instant_disk(
        &self,
        request: Request<proto::FinalizeInstantDiskRequest>,
    ) -> Result<Response<proto::FinalizeInstantDiskResponse>, Status> {
        auth::require_peer(&request, &[CN_CONTROLLER_PREFIX, CN_KCTL])?;
        let req = request.into_inner();
        let device = resolve_write_device(&req.backend_handle, &req.storage_class)?;
        let overlay = std::path::PathBuf::from(&req.overlay_path);
        let device_path = std::path::PathBuf::from(&device);
        let bytes = tokio::task::spawn_blocking(move || {
            crate::instant::copy_overlay(&overlay, &device_path)
        })
        .await
        .map_err(|e| Status::internal(format!("task join: {e}")))?
        .map_err(Status::internal)?;
        let vm_name = req.vm_name.trim().to_string();
        let swap = if vm_name.is_empty() {
            Ok(())
        } else {
            crate::instant::swap_disk(&vm_name, &device).await
        };
        let message = match swap {
            Ok(()) => "volume kept".to_string(),
            Err(e) => format!("volume written; disk swap: {e}"),
        };
        Ok(Response::new(proto::FinalizeInstantDiskResponse {
            success: true,
            message,
            bytes_written: bytes,
            volume_path: device,
        }))
    }
}

fn sanitize_instant_name(name: &str) -> String {
    let cleaned: String = name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.' {
                c
            } else {
                '-'
            }
        })
        .collect();
    if cleaned.is_empty() {
        "vault-instant".into()
    } else {
        cleaned
    }
}

fn lvm_to_storage(err: lvm_ops::LvmOpError) -> storage::StorageError {
    let kind = match err.kind {
        lvm_ops::Kind::InvalidArgument => storage::ErrorKind::InvalidArgument,
        lvm_ops::Kind::FailedPrecondition => storage::ErrorKind::FailedPrecondition,
        lvm_ops::Kind::Internal => storage::ErrorKind::Internal,
    };
    storage::StorageError::new(kind, err.message)
}

fn resolve_write_device(backend_handle: &str, storage_class: &str) -> Result<String, Status> {
    let class =
        export::resolve_storage_class(storage_class, backend_handle).map_err(Status::from)?;
    match class {
        export::StorageClass::Ceph => {
            let (pool, image) =
                export::validate_pool_image_handle(backend_handle).map_err(Status::from)?;
            crate::live_migrate::ensure_rbd_mapped(&pool, &image).map_err(Status::internal)?;
            Ok(format!("/dev/rbd/{pool}/{image}"))
        }
        export::StorageClass::Zfs => {
            let (pool, dataset) =
                export::validate_pool_image_handle(backend_handle).map_err(Status::from)?;
            Ok(format!("/dev/zvol/{pool}/{dataset}"))
        }
        export::StorageClass::Lvm => {
            let (vg, lv) =
                export::validate_pool_image_handle(backend_handle).map_err(Status::from)?;
            Ok(format!("/dev/{vg}/{lv}"))
        }
        export::StorageClass::Filesystem => {
            let path = backend_handle.trim();
            if !path.starts_with('/') {
                return Err(Status::invalid_argument(
                    "filesystem backend_handle must be an absolute path",
                ));
            }
            Ok(path.to_string())
        }
    }
}

/// Accept either `/dev/rbd/pool/image` or `pool/image` and ensure mapped.
fn resolve_rbd_device(rbd_device: &str) -> Result<String, String> {
    let s = rbd_device.trim();
    if let Some(rest) = s.strip_prefix("/dev/rbd/") {
        let (pool, image) = rest
            .split_once('/')
            .ok_or_else(|| "rbd_device must be /dev/rbd/<pool>/<image>".to_string())?;
        crate::live_migrate::ensure_rbd_mapped(pool, image)?;
        return Ok(s.to_string());
    }
    if let Some((pool, image)) = s.split_once('/') {
        crate::live_migrate::ensure_rbd_mapped(pool, image)?;
        return Ok(format!("/dev/rbd/{pool}/{image}"));
    }
    Err("rbd_device must be pool/image or /dev/rbd/pool/image".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn assert_denied(res: Result<impl Sized, Status>) {
        match res {
            Ok(_) => panic!("expected permission denied without TLS"),
            Err(err) => assert_eq!(err.code(), tonic::Code::PermissionDenied),
        }
    }

    #[tokio::test]
    async fn insecure_mode_denies_all_storage_endpoints() {
        let s = StorageService::new();

        assert_denied(
            <StorageService as proto::node_storage_server::NodeStorage>::create_volume(
                &s,
                Request::new(proto::CreateVolumeRequest {
                    volume_id: "vol-1".to_string(),
                    storage_class: "default".to_string(),
                    size_bytes: 1024,
                    parameters: std::collections::HashMap::new(),
                    lv_name: String::new(),
                }),
            )
            .await,
        );
        assert_denied(
            <StorageService as proto::node_storage_server::NodeStorage>::delete_volume(
                &s,
                Request::new(proto::DeleteVolumeRequest {
                    backend_handle: "/dev/null".to_string(),
                }),
            )
            .await,
        );
        assert_denied(
            <StorageService as proto::node_storage_server::NodeStorage>::attach_volume(
                &s,
                Request::new(proto::AttachVolumeRequest {
                    backend_handle: "/dev/null".to_string(),
                    vm_id: "vm-1".to_string(),
                    target_device: "vda".to_string(),
                    bus: "virtio".to_string(),
                }),
            )
            .await,
        );
        assert_denied(
            <StorageService as proto::node_storage_server::NodeStorage>::detach_volume(
                &s,
                Request::new(proto::DetachVolumeRequest {
                    backend_handle: "/dev/null".to_string(),
                    vm_id: "vm-1".to_string(),
                }),
            )
            .await,
        );
    }
}
