//! Handlers for `ConvertVmStorage` and `CopyVolumeToCluster`.
//!
//! A child of `controller.rs` so it can reach `ControllerService`'s private
//! state; the trait impl there only delegates here. Both move bytes the same
//! way: lease a source device on one node, read it in windows, and stream the
//! blocks into `WriteVolumeExtents` on the node that serves the destination.
//! The work runs in a spawned task so a client that disconnects does not
//! cancel it halfway and leave a guest stopped.

use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;
use tonic::{Request, Response, Status};
use tracing::{info, warn};
use uuid::Uuid;

use super::{ControllerService, EVT_VM_UPDATE, EVT_VOLUME_CREATE};
use crate::auth::OperatorRole;
use crate::controller_proto;
use crate::db::{NodeRow, VmOperationRow, VmRow, VolumeRow};
use crate::grpc::helpers::{status_with_context, vm_backend_handle};
use crate::node_proto;
use crate::volume_move as plan;
use kcore_sanitize::sanitize_nix_attr_key;

type StorageClient =
    node_proto::node_storage_client::NodeStorageClient<crate::node_client::NodeChannel>;

fn internal<E: std::fmt::Display>(e: E) -> Status {
    Status::internal(e.to_string())
}

struct Streamed {
    bytes_written: i64,
}

/// What a finished convert copy leaves for the caller to record.
struct Converted {
    row: VolumeRow,
    handle: String,
    bytes: i64,
}

fn human_bytes(bytes: i64) -> String {
    format!("{:.1} GiB", bytes as f64 / (1024.0 * 1024.0 * 1024.0))
}

fn split_handle(handle: &str) -> Result<(String, String), Status> {
    handle
        .trim()
        .split_once('/')
        .filter(|(p, i)| !p.is_empty() && !i.is_empty())
        .map(|(p, i)| (p.to_string(), i.to_string()))
        .ok_or_else(|| internal(format!("node returned a malformed RBD handle '{handle}'")))
}

impl ControllerService {
    async fn move_storage_client(&self, node: &NodeRow) -> Result<StorageClient, Status> {
        if let Some(client) = self.clients.get_storage(&node.address) {
            return Ok(client);
        }
        self.clients.connect(&node.address).await.map_err(|e| {
            Status::unavailable(format!("no connection to node {}: {e}", node.address))
        })?;
        self.clients
            .get_storage(&node.address)
            .ok_or_else(|| Status::unavailable(format!("no connection to node {}", node.address)))
    }

    /// Ready member of a healthy `cluster`, for RBD work against that cluster.
    fn healthy_member_of_cluster(&self, cluster: &str) -> Result<NodeRow, Status> {
        let row = self
            .db
            .get_ceph_cluster(cluster)
            .map_err(internal)?
            .ok_or_else(|| Status::not_found(format!("CephCluster '{cluster}' not found")))?;
        let healthy = self
            .db
            .get_ceph_cluster_status(cluster)
            .map_err(internal)?
            .is_some_and(|s| s.phase == "healthy");
        if !healthy {
            return Err(Status::failed_precondition(format!(
                "CephCluster '{cluster}' is not healthy"
            )));
        }
        let spec = crate::ceph_cluster_spec::spec_from_json(&row.spec_json).map_err(internal)?;
        for member in &spec.nodes {
            let Some(node) = self.db.get_node(&member.node_id).map_err(internal)? else {
                continue;
            };
            if crate::volume_export::node_ready(&node).is_ok() {
                return Ok(node);
            }
        }
        Err(Status::failed_precondition(format!(
            "CephCluster '{cluster}' has no ready member"
        )))
    }

    /// `(cluster, member node ids)` for every registered CephCluster.
    fn ceph_cluster_members(&self) -> Result<Vec<(String, Vec<String>)>, Status> {
        Ok(self
            .db
            .list_ceph_clusters()
            .map_err(internal)?
            .into_iter()
            .filter_map(|c| {
                crate::ceph_cluster_spec::spec_from_json(&c.spec_json)
                    .ok()
                    .map(|s| {
                        (
                            c.name,
                            s.nodes.into_iter().map(|n| n.node_id).collect::<Vec<_>>(),
                        )
                    })
            })
            .collect())
    }

    /// Read `total` bytes from `lease_id` on `source` and write them to
    /// `dest_handle` on `dest`. `on_progress` gets the end offset of each
    /// finished window.
    #[allow(clippy::too_many_arguments)]
    async fn stream_extents<F>(
        &self,
        source: &NodeRow,
        lease_id: &str,
        dest: &NodeRow,
        dest_handle: &str,
        total: i64,
        mut on_progress: F,
    ) -> Result<Streamed, Status>
    where
        F: FnMut(i64) + Send,
    {
        let mut reader = self.move_storage_client(source).await?;
        let mut writer = self.move_storage_client(dest).await?;
        let (tx, rx) = mpsc::channel::<node_proto::WriteVolumeExtentsRequest>(8);
        tx.send(node_proto::WriteVolumeExtentsRequest {
            msg: Some(node_proto::write_volume_extents_request::Msg::Target(
                node_proto::WriteVolumeTarget {
                    backend_handle: dest_handle.to_string(),
                    storage_class: "ceph".into(),
                    punch_zero_runs: false,
                },
            )),
        })
        .await
        .map_err(|_| Status::internal("failed to queue write target"))?;
        let write_task =
            tokio::spawn(async move { writer.write_volume_extents(ReceiverStream::new(rx)).await });

        let read_result: Result<(), Status> = async {
            for (offset, length) in plan::copy_windows(total, plan::COPY_WINDOW_BYTES) {
                let mut stream = reader
                    .read_volume_extents(node_proto::ReadVolumeExtentsRequest {
                        lease_id: lease_id.to_string(),
                        extents: vec![node_proto::Extent { offset, length }],
                        block_size: plan::COPY_BLOCK_BYTES,
                        elide_zero_blocks: true,
                    })
                    .await
                    .map_err(|e| status_with_context(&e, "reading the source disk"))?
                    .into_inner();
                while let Some(block) = stream
                    .message()
                    .await
                    .map_err(|e| status_with_context(&e, "reading the source disk"))?
                {
                    let msg = node_proto::WriteVolumeExtentsRequest {
                        msg: Some(node_proto::write_volume_extents_request::Msg::Block(block)),
                    };
                    if tx.send(msg).await.is_err() {
                        return Err(Status::aborted("the destination writer stopped"));
                    }
                }
                on_progress(offset + length);
            }
            Ok(())
        }
        .await;
        drop(tx);
        let written = write_task
            .await
            .map_err(|e| Status::internal(format!("task join: {e}")))?;
        match (read_result, written) {
            (_, Err(e)) => Err(status_with_context(&e, "writing the destination image")),
            (Err(e), Ok(_)) => Err(e),
            (Ok(()), Ok(resp)) => Ok(Streamed {
                bytes_written: resp.into_inner().bytes_written,
            }),
        }
    }

    /// Unmap and delete an RBD image this request created. Best effort.
    async fn discard_new_image(&self, node: &NodeRow, handle: &str, vm_id: &str) {
        let Ok(mut storage) = self.move_storage_client(node).await else {
            warn!(handle, "cannot reach node to discard the new RBD image");
            return;
        };
        let _ = storage
            .detach_volume(node_proto::DetachVolumeRequest {
                backend_handle: handle.to_string(),
                vm_id: vm_id.to_string(),
            })
            .await;
        if let Err(e) = storage
            .delete_volume(node_proto::DeleteVolumeRequest {
                backend_handle: handle.to_string(),
            })
            .await
        {
            warn!(handle, error = %e, "failed to delete the partial RBD image");
        }
    }

    pub(super) async fn convert_vm_storage_rpc(
        &self,
        request: Request<controller_proto::ConvertVmStorageRequest>,
    ) -> Result<Response<controller_proto::ConvertVmStorageResponse>, Status> {
        self.require_operator(&request, OperatorRole::VmAdmin)?;
        let actor = Self::audit_actor(&request);
        let req = request.into_inner();
        plan::parse_target(&req.to).map_err(Status::invalid_argument)?;
        let vm = self.resolve_vm(req.vm.trim())?;
        let node = self
            .db
            .get_node(&vm.node_id)
            .map_err(internal)?
            .ok_or_else(|| Status::failed_precondition("VM node missing"))?;
        crate::volume_export::node_ready(&node).map_err(Status::failed_precondition)?;
        let member = self.is_healthy_ceph_member(&node.id)?;
        let running = vm.auto_start || self.vm_is_live(&vm).await?;
        let convert = plan::plan_convert(
            &vm.storage_backend,
            running,
            req.allow_stop,
            member,
            &node.id,
        )
        .map_err(Status::failed_precondition)?;
        if self
            .db
            .get_volume_by_vm(&vm.id)
            .map_err(internal)?
            .is_some()
        {
            return Err(Status::failed_precondition(format!(
                "VM '{}' already has a root volume row",
                vm.name
            )));
        }
        if let Some(open) = self.db.get_open_vm_operation(&vm.id).map_err(internal)? {
            return Err(Status::failed_precondition(format!(
                "VM '{}' already has open operation {} in phase {}",
                vm.name, open.id, open.phase
            )));
        }
        let cluster =
            plan::cluster_for_node(&self.ceph_cluster_members()?, &node.id).unwrap_or_default();
        let op_id = Uuid::new_v4().to_string();
        self.db
            .insert_vm_operation(&VmOperationRow {
                id: op_id.clone(),
                vm_id: vm.id.clone(),
                kind: VmOperationRow::KIND_CONVERT_STORAGE.into(),
                phase: VmOperationRow::PHASE_PREPARING.into(),
                source_node: node.id.clone(),
                target_node: node.id.clone(),
                cancel_requested: false,
                send_succeeded: false,
                detail_json: "{}".into(),
                started_at: String::new(),
                updated_at: String::new(),
                finished_at: String::new(),
            })
            .map_err(|e| Status::internal(format!("recording convert operation: {e}")))?;

        let svc = self.clone();
        let task_vm = vm.clone();
        let task_op = op_id.clone();
        let outcome = tokio::spawn(async move {
            svc.convert_vm_storage_run(&actor, task_vm, node, cluster, &task_op, convert)
                .await
        })
        .await
        .map_err(|e| Status::internal(format!("task join: {e}")))?;
        match outcome {
            Ok(done) => Ok(Response::new(controller_proto::ConvertVmStorageResponse {
                success: true,
                message: format!(
                    "VM '{}' now runs on Ceph ({} copied). The local disk {} was left in place; \
                     delete it once the guest checks out",
                    vm.name,
                    human_bytes(done.bytes),
                    done.handle_source
                ),
                operation_id: op_id,
                bytes_total: done.bytes,
                bytes_copied: done.bytes,
                source_disk: done.handle_source,
            })),
            Err(status) => {
                let _ = self.db.update_vm_operation_phase(
                    &op_id,
                    VmOperationRow::PHASE_FAILED,
                    Some(false),
                    Some(&serde_json::json!({ "error": status.message() }).to_string()),
                    true,
                );
                Err(status)
            }
        }
    }
}

struct ConvertDone {
    bytes: i64,
    handle_source: String,
}

impl ControllerService {
    async fn convert_vm_storage_run(
        &self,
        actor: &str,
        vm: VmRow,
        node: NodeRow,
        cluster: String,
        op_id: &str,
        convert: plan::ConvertPlan,
    ) -> Result<ConvertDone, Status> {
        let runtime_name = sanitize_nix_attr_key(&vm.name);
        let source_handle = vm_backend_handle(&vm);
        let original_backend = vm.storage_backend.clone();
        let wanted_running = vm.auto_start;
        info!(vm = %vm.name, node = %node.id, stop = convert.stop_guest, "converting VM storage to ceph");

        // The unit is stopped and cannot restart until the hold is cleared;
        // the desired state is cleared too so HA does not start it again.
        self.set_local_quiesced(&node, &runtime_name, true, false)
            .await?;
        if let Err(e) = self.db.set_vm_auto_start(&vm.id, false) {
            let _ = self
                .set_local_quiesced(&node, &runtime_name, false, wanted_running)
                .await;
            return Err(internal(e));
        }
        let restore = |this: &Self| {
            let this = this.clone();
            let node = node.clone();
            let name = runtime_name.clone();
            let vm_id = vm.id.clone();
            async move {
                let _ = this.db.set_vm_auto_start(&vm_id, wanted_running);
                let _ = this
                    .set_local_quiesced(&node, &name, false, wanted_running)
                    .await;
            }
        };

        let _ = self.db.update_vm_operation_phase(
            op_id,
            VmOperationRow::PHASE_COPYING,
            None,
            None,
            false,
        );
        let copied = match self
            .convert_copy(&vm, &node, &cluster, &source_handle, op_id)
            .await
        {
            Ok(c) => c,
            Err(e) => {
                restore(self).await;
                return Err(e);
            }
        };

        let _ = self.db.update_vm_operation_phase(
            op_id,
            VmOperationRow::PHASE_FINALIZING_DEST,
            None,
            None,
            false,
        );
        if let Err(e) = self
            .set_local_quiesced(&node, &runtime_name, false, false)
            .await
        {
            warn!(vm = %vm.name, error = %e, "could not clear the restart hold before the flip");
        }
        if let Err(e) = self.db.set_vm_storage_backend(&vm.id, "ceph") {
            self.discard_new_image(&node, &copied.handle, &vm.id).await;
            restore(self).await;
            return Err(internal(e));
        }
        if let Err(e) = self.db.upsert_volume(&copied.row) {
            let _ = self.db.set_vm_storage_backend(&vm.id, &original_backend);
            self.discard_new_image(&node, &copied.handle, &vm.id).await;
            restore(self).await;
            return Err(internal(e));
        }
        let applied = if wanted_running {
            self.set_vm_desired_state_internal(&vm.id, "", true)
                .await
                .map(|_| ())
        } else {
            self.push_config_and_await_apply(&node).await
        };
        if let Err(e) = applied {
            let _ = self.db.set_vm_storage_backend(&vm.id, &original_backend);
            let _ = self.db.delete_root_volume_by_vm(&vm.id);
            let _ = self.db.set_vm_auto_start(&vm.id, wanted_running);
            if let Err(push) = self.push_config_to_node(&node).await {
                warn!(vm = %vm.name, error = %push, "rollback: could not push the previous config");
            }
            self.discard_new_image(&node, &copied.handle, &vm.id).await;
            let _ = self
                .set_local_quiesced(&node, &runtime_name, false, wanted_running)
                .await;
            return Err(Status::aborted(format!(
                "the copy finished but node {} did not apply the Ceph config: {}; \
                 the VM was returned to its local disk",
                node.id,
                e.message()
            )));
        }

        self.log_replication_event(
            actor,
            Some("ConvertVmStorage"),
            EVT_VOLUME_CREATE,
            &format!("volume/{}", copied.row.name),
            serde_json::json!({
                "id": copied.row.id,
                "name": copied.row.name,
                "role": copied.row.role,
                "pool": copied.row.pool,
                "image": copied.row.image,
                "sizeBytes": copied.row.size_bytes,
                "vmId": copied.row.vm_id,
                "attachState": copied.row.attach_state,
                "serial": copied.row.serial,
                "slot": copied.row.slot,
                "storageClass": copied.row.storage_class,
                "sourceJson": copied.row.source_json,
            }),
        );
        self.log_replication_event(
            actor,
            None,
            EVT_VM_UPDATE,
            &format!("vm/{}", vm.id),
            serde_json::json!({
                "vmId": vm.id,
                "nodeId": vm.node_id,
                "storageBackend": "ceph",
            }),
        );
        let _ = self.db.update_vm_operation_phase(
            op_id,
            VmOperationRow::PHASE_DONE,
            Some(true),
            Some(&plan::progress_detail(copied.bytes, copied.bytes)),
            true,
        );
        Ok(ConvertDone {
            bytes: copied.bytes,
            handle_source: source_handle,
        })
    }

    /// Lease the stopped guest's disk, create the RBD image, copy, and mark
    /// the image seeded so the VM unit never writes the OS image over it.
    async fn convert_copy(
        &self,
        vm: &VmRow,
        node: &NodeRow,
        cluster: &str,
        source_handle: &str,
        op_id: &str,
    ) -> Result<Converted, Status> {
        let mut storage = self.move_storage_client(node).await?;
        let lease = storage
            .expose_local_disk(node_proto::ExposeLocalDiskRequest {
                backend_handle: source_handle.to_string(),
                storage_class: vm.storage_backend.clone(),
                lease_seconds: plan::COPY_LEASE_SECONDS,
            })
            .await
            .map_err(|e| status_with_context(&e, "exposing the local disk"))?
            .into_inner();
        let release = |mut s: StorageClient, lease_id: String| async move {
            let _ = s
                .unexpose_snapshot(node_proto::UnexposeSnapshotRequest { lease_id })
                .await;
        };

        let bytes = lease.size_bytes;
        let mut row = VolumeRow::new_root(&vm.id, &vm.name, bytes);
        row.source_json = plan::stamp_source_json("{}", cluster, None);
        let created = storage
            .create_volume(node_proto::CreateVolumeRequest {
                volume_id: row.image.clone(),
                storage_class: "ceph".into(),
                size_bytes: plan::rbd_image_bytes(bytes),
                parameters: Default::default(),
                lv_name: String::new(),
            })
            .await;
        let handle = match created {
            Ok(r) => r.into_inner().backend_handle,
            Err(e) => {
                release(storage.clone(), lease.lease_id.clone()).await;
                return Err(status_with_context(&e, "creating the RBD image"));
            }
        };
        let (pool, image) = match split_handle(&handle) {
            Ok(p) => p,
            Err(e) => {
                release(storage.clone(), lease.lease_id.clone()).await;
                self.discard_new_image(node, &handle, &vm.id).await;
                return Err(e);
            }
        };
        row.pool = pool;
        row.image = image;

        let db = self.db.clone();
        let progress_op = op_id.to_string();
        let streamed = self
            .stream_extents(node, &lease.lease_id, node, &handle, bytes, move |done| {
                let _ = db.update_vm_operation_phase(
                    &progress_op,
                    VmOperationRow::PHASE_COPYING,
                    None,
                    Some(&plan::progress_detail(done, bytes)),
                    false,
                );
            })
            .await;
        release(storage.clone(), lease.lease_id.clone()).await;
        let finish = match streamed {
            Ok(s) if plan::copy_complete(bytes, s.bytes_written) => Ok(()),
            Ok(s) => Err(Status::data_loss(format!(
                "wrote {} of {bytes} bytes; the destination image was discarded",
                s.bytes_written
            ))),
            Err(e) => Err(e),
        };
        let finish = match finish {
            Ok(()) => storage
                .set_image_meta(node_proto::SetImageMetaRequest {
                    backend_handle: handle.clone(),
                    key: "kcore.seeded".into(),
                    value: "1".into(),
                })
                .await
                .map(|_| ())
                .map_err(|e| status_with_context(&e, "marking the image seeded")),
            Err(e) => Err(e),
        };
        if let Err(e) = finish {
            self.discard_new_image(node, &handle, &vm.id).await;
            return Err(e);
        }
        // Release the nbd mapping so the VM unit maps the image with its own
        // cache policy.
        let _ = storage
            .detach_volume(node_proto::DetachVolumeRequest {
                backend_handle: handle.clone(),
                vm_id: vm.id.clone(),
            })
            .await;
        Ok(Converted { row, handle, bytes })
    }

    pub(super) async fn copy_volume_to_cluster_rpc(
        &self,
        request: Request<controller_proto::CopyVolumeToClusterRequest>,
    ) -> Result<Response<controller_proto::CopyVolumeToClusterResponse>, Status> {
        self.require_operator(&request, OperatorRole::VmAdmin)?;
        let actor = Self::audit_actor(&request);
        let req = request.into_inner();
        let src = self
            .resolve_volume_row(req.name.trim())?
            .ok_or_else(|| Status::not_found(format!("volume '{}' not found", req.name)))?;
        if !matches!(src.storage_class.trim(), "" | "ceph") {
            return Err(Status::failed_precondition(
                "only Ceph volumes can be copied between clusters",
            ));
        }
        if src.encrypted {
            return Err(Status::failed_precondition(
                "copying an encrypted volume is not supported yet",
            ));
        }
        let new_name =
            plan::copy_target_name(&src.name, &req.new_name).map_err(Status::invalid_argument)?;
        if self
            .db
            .get_volume_by_name(&new_name)
            .map_err(internal)?
            .is_some()
        {
            return Err(Status::already_exists(format!(
                "volume '{new_name}' already exists"
            )));
        }
        let to_cluster = req.to_cluster.trim().to_string();
        if to_cluster.is_empty() {
            return Err(Status::invalid_argument("to_cluster is required"));
        }
        let names: Vec<String> = self
            .db
            .list_ceph_clusters()
            .map_err(internal)?
            .into_iter()
            .map(|c| c.name)
            .collect();
        if !names.contains(&to_cluster) {
            return Err(Status::not_found(format!(
                "CephCluster '{to_cluster}' not found"
            )));
        }
        let from_cluster =
            plan::resolve_source_cluster(&req.from_cluster, &src.source_json, &names)
                .map_err(Status::failed_precondition)?;
        plan::plan_copy(&from_cluster, &to_cluster).map_err(Status::failed_precondition)?;
        let source_node = self.healthy_member_of_cluster(&from_cluster)?;
        let dest_node = self.healthy_member_of_cluster(&to_cluster)?;

        let svc = self.clone();
        let outcome = tokio::spawn(async move {
            svc.copy_volume_run(&actor, src, new_name, to_cluster, source_node, dest_node)
                .await
        })
        .await
        .map_err(|e| Status::internal(format!("task join: {e}")))??;
        Ok(Response::new(outcome))
    }

    async fn copy_volume_run(
        &self,
        actor: &str,
        src: VolumeRow,
        new_name: String,
        to_cluster: String,
        source_node: NodeRow,
        dest_node: NodeRow,
    ) -> Result<controller_proto::CopyVolumeToClusterResponse, Status> {
        let mut source = self.move_storage_client(&source_node).await?;
        let mut dest = self.move_storage_client(&dest_node).await?;
        let source_handle = format!("{}/{}", src.pool, src.image);
        let snap = format!("kcore-copy-{}", &Uuid::new_v4().simple().to_string()[..8]);
        info!(volume = %src.name, to = %to_cluster, snapshot = %snap, "copying volume between clusters");

        source
            .snapshot_volume(node_proto::SnapshotVolumeRequest {
                backend_handle: source_handle.clone(),
                snapshot_name: snap.clone(),
                protect: false,
                storage_class: "ceph".into(),
            })
            .await
            .map_err(|e| status_with_context(&e, "snapshotting the source volume"))?;
        let drop_snapshot = |mut s: StorageClient, handle: String, snap: String| async move {
            if let Err(e) = s
                .delete_volume_snapshot(node_proto::DeleteVolumeSnapshotRequest {
                    backend_handle: handle,
                    snapshot_name: snap.clone(),
                    unprotect: false,
                    storage_class: "ceph".into(),
                })
                .await
            {
                warn!(snapshot = %snap, error = %e, "failed to delete the copy snapshot");
            }
        };

        let lease = match source
            .expose_snapshot(node_proto::ExposeSnapshotRequest {
                backend_handle: source_handle.clone(),
                snapshot_name: snap.clone(),
                storage_class: "ceph".into(),
                lease_seconds: plan::COPY_LEASE_SECONDS,
            })
            .await
        {
            Ok(r) => r.into_inner(),
            Err(e) => {
                drop_snapshot(source.clone(), source_handle.clone(), snap.clone()).await;
                return Err(status_with_context(&e, "exposing the source snapshot"));
            }
        };
        let bytes = if lease.size_bytes > 0 {
            lease.size_bytes
        } else {
            src.size_bytes
        };
        let release = |mut s: StorageClient, lease_id: String| async move {
            let _ = s
                .unexpose_snapshot(node_proto::UnexposeSnapshotRequest { lease_id })
                .await;
        };

        let mut row = VolumeRow::new_data(&new_name, bytes);
        row.guest_format_json = src.guest_format_json.clone();
        row.source_json = plan::stamp_source_json("{}", &to_cluster, Some(&src.id));
        let created = dest
            .create_volume(node_proto::CreateVolumeRequest {
                volume_id: row.image.clone(),
                storage_class: "ceph".into(),
                size_bytes: plan::rbd_image_bytes(bytes),
                parameters: Default::default(),
                lv_name: String::new(),
            })
            .await;
        let handle = match created {
            Ok(r) => r.into_inner().backend_handle,
            Err(e) => {
                release(source.clone(), lease.lease_id.clone()).await;
                drop_snapshot(source.clone(), source_handle.clone(), snap.clone()).await;
                return Err(status_with_context(&e, "creating the destination image"));
            }
        };
        let (pool, image) = match split_handle(&handle) {
            Ok(p) => p,
            Err(e) => {
                release(source.clone(), lease.lease_id.clone()).await;
                drop_snapshot(source.clone(), source_handle.clone(), snap.clone()).await;
                self.discard_new_image(&dest_node, &handle, &row.id).await;
                return Err(e);
            }
        };
        row.pool = pool;
        row.image = image;

        let streamed = self
            .stream_extents(
                &source_node,
                &lease.lease_id,
                &dest_node,
                &handle,
                bytes,
                |_| {},
            )
            .await;
        release(source.clone(), lease.lease_id.clone()).await;
        drop_snapshot(source.clone(), source_handle.clone(), snap.clone()).await;
        let finish = match streamed {
            Ok(s) if plan::copy_complete(bytes, s.bytes_written) => Ok(()),
            Ok(s) => Err(Status::data_loss(format!(
                "wrote {} of {bytes} bytes; the destination image was discarded",
                s.bytes_written
            ))),
            Err(e) => Err(e),
        };
        let finish = match finish {
            Ok(()) if src.role == VolumeRow::ROLE_ROOT => dest
                .set_image_meta(node_proto::SetImageMetaRequest {
                    backend_handle: handle.clone(),
                    key: "kcore.seeded".into(),
                    value: "1".into(),
                })
                .await
                .map(|_| ())
                .map_err(|e| status_with_context(&e, "marking the image seeded")),
            other => other,
        };
        if let Err(e) = finish {
            self.discard_new_image(&dest_node, &handle, &row.id).await;
            return Err(e);
        }
        let _ = dest
            .detach_volume(node_proto::DetachVolumeRequest {
                backend_handle: handle.clone(),
                vm_id: row.id.clone(),
            })
            .await;
        if let Err(e) = self.db.upsert_volume(&row) {
            self.discard_new_image(&dest_node, &handle, &row.id).await;
            return Err(Status::internal(format!("storing volume: {e}")));
        }
        self.log_replication_event(
            actor,
            Some("CopyVolumeToCluster"),
            EVT_VOLUME_CREATE,
            &format!("volume/{}", row.name),
            serde_json::json!({
                "id": row.id,
                "name": row.name,
                "role": row.role,
                "pool": row.pool,
                "image": row.image,
                "sizeBytes": row.size_bytes,
                "vmId": row.vm_id,
                "attachState": row.attach_state,
                "serial": row.serial,
                "slot": row.slot,
                "storageClass": row.storage_class,
                "sourceJson": row.source_json,
            }),
        );
        Ok(controller_proto::CopyVolumeToClusterResponse {
            success: true,
            message: format!(
                "copied volume '{}' to '{}' on CephCluster '{}'",
                src.name, row.name, to_cluster
            ),
            volume: Some(self.volume_info_from_row(&row, None)),
            bytes_total: bytes,
            bytes_copied: bytes,
        })
    }
}
