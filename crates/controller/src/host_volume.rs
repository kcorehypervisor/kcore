//! Controller-side names for LVM and ZFS volumes that live on one node.

pub fn is_host(storage_class: &str) -> bool {
    matches!(storage_class, "lvm" | "zfs")
}

pub fn require_ceph(storage_class: &str, action: &str) -> Result<(), String> {
    if is_host(storage_class) {
        Err(format!("{action} is only supported for ceph volumes"))
    } else {
        Ok(())
    }
}

pub fn device_path(storage_class: &str, pool: &str, image: &str) -> Result<String, String> {
    if !component_ok(pool) || !component_ok(image) {
        return Err("volume is missing its pool or image name".into());
    }
    match storage_class {
        "lvm" => Ok(format!("/dev/{pool}/{image}")),
        "zfs" => Ok(format!("/dev/zvol/{pool}/{image}")),
        other => Err(format!("not a host volume class: {other}")),
    }
}

pub fn split_backend_handle(storage_class: &str, handle: &str) -> Result<(String, String), String> {
    let handle = handle.trim();
    let rest = match storage_class {
        "lvm" => handle
            .strip_prefix("/dev/")
            .ok_or_else(|| format!("lvm handle must be /dev/<vg>/<lv>, got {handle}"))?,
        "zfs" => handle.strip_prefix("/dev/zvol/").ok_or_else(|| {
            format!("zfs handle must be /dev/zvol/<pool>/<dataset>, got {handle}")
        })?,
        other => return Err(format!("not a host volume class: {other}")),
    };
    let (pool, image) = rest
        .split_once('/')
        .filter(|(pool, image)| component_ok(pool) && component_ok(image))
        .ok_or_else(|| format!("handle {handle} must name one volume in one pool"))?;
    Ok((pool.to_string(), image.to_string()))
}

fn component_ok(value: &str) -> bool {
    !value.is_empty()
        && !value.contains('/')
        && !value.contains('\\')
        && !value.contains('\0')
        && value != "."
        && value != ".."
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lvm_and_zfs_handles_round_trip() {
        let lvm = device_path("lvm", "vg_kcore", "k8s-local").unwrap();
        assert_eq!(
            split_backend_handle("lvm", &lvm).unwrap(),
            ("vg_kcore".into(), "k8s-local".into())
        );
        let zfs = device_path("zfs", "tank0", "k8s-local").unwrap();
        assert_eq!(zfs, "/dev/zvol/tank0/k8s-local");
        assert_eq!(
            split_backend_handle("zfs", &zfs).unwrap(),
            ("tank0".into(), "k8s-local".into())
        );
    }

    #[test]
    fn rejects_a_handle_outside_the_class() {
        assert!(split_backend_handle("lvm", "/dev/zvol/tank0/data").is_err());
        assert!(split_backend_handle("zfs", "/dev/vg_kcore/data").is_err());
        assert!(split_backend_handle("lvm", "/dev/vg_kcore/../sda").is_err());
        assert!(require_ceph("zfs", "snapshot").is_err());
        assert!(require_ceph("ceph", "snapshot").is_ok());
    }
}
