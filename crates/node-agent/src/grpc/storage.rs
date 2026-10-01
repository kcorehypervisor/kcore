use tonic::{Request, Response, Status};

use crate::auth::{self, CN_CONTROLLER_PREFIX, CN_KCTL};
use crate::proto;
use crate::storage::{self, StorageAdapter};
use std::sync::Arc;

pub struct StorageService {
    storage: Arc<dyn StorageAdapter>,
}

impl StorageService {
    pub fn new() -> Self {
        Self {
            storage: storage::default_adapter(),
        }
    }

    pub fn new_with_storage(storage: Arc<dyn StorageAdapter>) -> Self {
        Self { storage }
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
            storage::ceph_snapshot_volume(&req.backend_handle, &req.snapshot_name, req.protect)
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
            storage::ceph_delete_snapshot(&req.backend_handle, &req.snapshot_name, req.unprotect)
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
            storage::ceph_clone_volume(&req.parent_handle, &req.parent_snapshot, &req.child_image)
                .map(|backend_handle| proto::CloneVolumeResponse { backend_handle })
        })
        .await
        .map_err(|e| Status::internal(format!("task join: {e}")))??;
        Ok(Response::new(resp))
    }

    async fn rollback_volume(
        &self,
        request: Request<proto::RollbackVolumeRequest>,
    ) -> Result<Response<proto::RollbackVolumeResponse>, Status> {
        auth::require_peer(&request, &[CN_CONTROLLER_PREFIX, CN_KCTL])?;
        let req = request.into_inner();
        tokio::task::spawn_blocking(move || {
            storage::ceph_rollback_volume(&req.backend_handle, &req.snapshot_name)
        })
        .await
        .map_err(|e| Status::internal(format!("task join: {e}")))??;
        Ok(Response::new(proto::RollbackVolumeResponse {}))
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
        let size = tokio::task::spawn_blocking(move || {
            storage::ceph_resize_volume(&req.backend_handle, req.size_bytes, allow_shrink)
        })
        .await
        .map_err(|e| Status::internal(format!("task join: {e}")))??;
        Ok(Response::new(proto::ResizeVolumeResponse {
            success: true,
            message: format!("resized to {size} bytes"),
            size_bytes: size,
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
