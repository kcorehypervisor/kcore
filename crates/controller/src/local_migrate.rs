//! Planning for moving an LVM or ZFS VM without shared storage.
//!
//! The node agent does the copy (`volume_mirror`). This module decides whether
//! that copy can run under the guest (live) or has to stop it first (cold),
//! and owns the names the generated Nix configuration also uses.

use crate::db::VmOperationRow;

/// Volume group the controller writes into every LVM node's Nix config.
/// `vm_backend_handle` in `grpc/helpers.rs` and `lvName` in `vm-service.nix`
/// must keep using this name.
pub const LVM_VG: &str = "vg_kcore";

/// Pool the controller writes into every ZFS node's Nix config.
pub const ZFS_POOL: &str = "tank0";

/// Line in `/etc/kcore/ch-vm-features` once the unit creates the stable
/// device-mapper path and boots with `shared=on`.
pub const FEATURE: &str = "localLiveMigrate";

/// Backends whose disk is a node-local block device that can be mirrored.
pub fn is_local_block(backend: &str) -> bool {
    backend == "lvm" || backend == "zfs"
}

/// How the disk moves. Memory migration is a separate step that only follows
/// [`LocalMove::Live`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LocalMove {
    /// Guest stays running; dm-mirror resyncs, then Cloud Hypervisor migrates.
    Live,
    /// Guest is stopped, then the disk is copied.
    ColdCopy,
}

/// `running` guests can move live only when they already opened the stable
/// mapper with shared memory. Anything else is a cold copy when the caller
/// accepts the downtime, and a refusal otherwise.
pub fn plan_local_move(
    running: bool,
    guest_uses_mapper: bool,
    memory_shared: bool,
    allow_cold: bool,
) -> Result<LocalMove, String> {
    if running && guest_uses_mapper && memory_shared {
        return Ok(LocalMove::Live);
    }
    if !running || allow_cold {
        return Ok(LocalMove::ColdCopy);
    }
    Err(
        "this VM is running on a node-local disk that is not ready for live migration. \
         Restart it once so it uses /dev/mapper/kcore-vm-<name> and shared memory, \
         or pass --allow-cold-fallback to stop the guest and copy the disk"
            .into(),
    )
}

pub fn mirror_detail(bytes_synced: i64, bytes_total: i64, in_sync: bool) -> String {
    format!(
        r#"{{"mirror":{{"bytesSynced":{bytes_synced},"bytesTotal":{bytes_total},"inSync":{in_sync}}}}}"#
    )
}

/// Phases during which a local-disk move may still be cancelled: the guest
/// has not been sent, and (for a cold copy) the source disk has not been deleted.
pub fn local_phase_is_open(phase: &str) -> bool {
    phase == VmOperationRow::PHASE_MIRROR_SYNCING || phase == VmOperationRow::PHASE_COPYING
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_running_guest_on_the_mapper_moves_live() {
        assert_eq!(
            plan_local_move(true, true, true, false).unwrap(),
            LocalMove::Live
        );
    }

    #[test]
    fn a_running_guest_on_the_old_path_needs_an_explicit_cold_copy() {
        let err = plan_local_move(true, false, false, false).unwrap_err();
        assert!(err.contains("allow-cold-fallback"), "{err}");
        assert_eq!(
            plan_local_move(true, false, false, true).unwrap(),
            LocalMove::ColdCopy
        );
    }

    #[test]
    fn a_stopped_guest_is_copied_without_the_cold_flag() {
        assert_eq!(
            plan_local_move(false, false, false, false).unwrap(),
            LocalMove::ColdCopy
        );
    }

    #[test]
    fn shared_memory_without_the_mapper_is_not_live() {
        assert_eq!(
            plan_local_move(true, false, true, true).unwrap(),
            LocalMove::ColdCopy
        );
    }

    #[test]
    fn mirror_detail_is_json() {
        let detail = mirror_detail(5, 10, false);
        let v: serde_json::Value = serde_json::from_str(&detail).unwrap();
        assert_eq!(v["mirror"]["bytesSynced"], 5);
        assert_eq!(v["mirror"]["inSync"], false);
    }
}
