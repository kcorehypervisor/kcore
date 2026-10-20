//! Command lines for an asynchronous ZFS replica of one VM volume.
//!
//! The send is incremental once both sides share a snapshot. `zfs recv -F`
//! rolls the replica back to that snapshot, which is the recovery point the
//! failover start uses.

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SendPlan {
    pub dataset: String,
    pub snapshot: String,
    pub send_args: Vec<String>,
    pub recv_args: Vec<String>,
}

pub fn dataset_name(pool: &str, prefix: &str, vm: &str) -> Result<String, String> {
    let pool = pool.trim();
    let vm = vm.trim();
    if pool.is_empty() || vm.is_empty() {
        return Err("zfs pool and vm name are required".into());
    }
    if pool.contains([' ', '@', '/']) || vm.contains([' ', '@', '/']) {
        return Err("zfs pool and vm name must be a single path component".into());
    }
    let prefix = if prefix.is_empty() { "kcore-" } else { prefix };
    Ok(format!("{pool}/{prefix}{vm}"))
}

pub fn plan_send(
    pool: &str,
    prefix: &str,
    vm: &str,
    incremental_from: &str,
    unix: i64,
) -> Result<SendPlan, String> {
    let dataset = dataset_name(pool, prefix, vm)?;
    let snap = format!("kcore-ha-{unix}");
    let full = format!("{dataset}@{snap}");
    let mut send_args = vec!["send".to_string()];
    let from = incremental_from.trim();
    if !from.is_empty() {
        if from.contains([' ', ';']) {
            return Err("incremental snapshot name is not safe".into());
        }
        send_args.push("-i".into());
        send_args.push(format!("{dataset}@{from}"));
    }
    send_args.push(full);
    let recv_args = vec!["recv".into(), "-F".into(), "-u".into(), dataset.clone()];
    Ok(SendPlan {
        dataset,
        snapshot: snap,
        send_args,
        recv_args,
    })
}

/// Touch the seed marker and record the snapshot the replica was received at.
/// The VM unit rolls back to that snapshot and skips `qemu-img convert` when
/// the seed marker is present.
pub fn write_replica_markers(vm: &str, snapshot: &str) -> Result<(), String> {
    if !safe_component(vm) || snapshot.is_empty() || snapshot.contains(['/', ' ', ';']) {
        return Err("vm name or snapshot is not a single path component".into());
    }
    let seeded_dir = std::path::Path::new("/var/lib/kcore/volume-seeded");
    let ready_dir = std::path::Path::new("/var/lib/kcore/replica-ready");
    std::fs::create_dir_all(seeded_dir).map_err(|e| format!("creating seed dir: {e}"))?;
    std::fs::create_dir_all(ready_dir).map_err(|e| format!("creating replica dir: {e}"))?;
    std::fs::write(seeded_dir.join(vm), b"").map_err(|e| format!("writing seed marker: {e}"))?;
    std::fs::write(ready_dir.join(vm), snapshot.as_bytes())
        .map_err(|e| format!("writing replica marker: {e}"))?;
    Ok(())
}

fn safe_component(value: &str) -> bool {
    !value.is_empty() && !value.contains(['/', '\\', ' ']) && !value.contains("..")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn full_and_incremental_sends() {
        let full = plan_send("tank0", "kcore-", "web", "", 100).unwrap();
        assert_eq!(full.dataset, "tank0/kcore-web");
        assert_eq!(full.send_args, vec!["send", "tank0/kcore-web@kcore-ha-100"]);
        assert_eq!(full.recv_args, vec!["recv", "-F", "-u", "tank0/kcore-web"]);
        let inc = plan_send("tank0", "kcore-", "web", "kcore-ha-100", 200).unwrap();
        assert_eq!(
            inc.send_args,
            vec![
                "send",
                "-i",
                "tank0/kcore-web@kcore-ha-100",
                "tank0/kcore-web@kcore-ha-200"
            ]
        );
    }

    #[test]
    fn names_with_spaces_are_rejected() {
        assert!(dataset_name("tank 0", "kcore-", "web").is_err());
        assert!(plan_send("tank0", "kcore-", "web", "bad snap", 1).is_err());
    }
}
