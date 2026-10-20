//! Host-side high availability: placement confirmation and isolation.
//!
//! A VM unit does not start until `kcore-placement-confirmed.target` is
//! reached. The agent reaches it after the controller lists the VMs that
//! still belong here, and masks every Ceph unit the controller has moved
//! away. If no controller answers, local-disk VMs are allowed to start and
//! Ceph units stay masked: nobody else can run a local disk, and a Ceph
//! disk might already be running somewhere else.

use std::path::Path;
use std::time::{Duration, Instant};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LocalVm {
    pub name: String,
    pub backend: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HostAction {
    Mask(String),
    Unmask(String),
    StartTarget,
    Stop(String),
    Restart(String),
}

pub fn parse_placement_file(text: &str) -> Vec<LocalVm> {
    text.lines()
        .filter_map(|line| {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                return None;
            }
            let mut parts = line.split_whitespace();
            let name = parts.next()?.to_string();
            let backend = parts.next().unwrap_or("filesystem").to_string();
            Some(LocalVm { name, backend })
        })
        .collect()
}

/// What to do with the units in the booted configuration.
///
/// `confirmed` false means the controller is too old to say, so every unit
/// may start. After `timed_out`, Ceph units that were never confirmed are
/// masked and the target is started so local disks boot.
pub fn placement_actions(
    local: &[LocalVm],
    assigned: &[String],
    confirmed: bool,
    timed_out: bool,
) -> Vec<HostAction> {
    if !confirmed && !timed_out {
        return Vec::new();
    }
    let mut actions = Vec::new();
    for vm in local {
        let owned = !confirmed || assigned.iter().any(|id| id == &vm.name);
        if vm.backend == "ceph" && !owned {
            actions.push(HostAction::Mask(vm.name.clone()));
        } else {
            actions.push(HostAction::Unmask(vm.name.clone()));
        }
    }
    actions.push(HostAction::StartTarget);
    actions
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StorageView {
    ReachableNotFenced,
    ReachableFenced,
    Unreachable,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IsolationResponse {
    PowerOff,
    LeaveRunning,
}

pub fn parse_isolation_response(raw: &str) -> IsolationResponse {
    if raw.trim().eq_ignore_ascii_case("leaveRunning") {
        IsolationResponse::LeaveRunning
    } else {
        IsolationResponse::PowerOff
    }
}

/// Power off protected guests only when their images are fenced or Ceph
/// itself is unreachable. A controller-only partition leaves the guests
/// running: they are still the only writers.
pub fn isolation_actions(
    local: &[LocalVm],
    view: StorageView,
    response: IsolationResponse,
) -> Vec<HostAction> {
    if response == IsolationResponse::LeaveRunning {
        return Vec::new();
    }
    if view == StorageView::ReachableNotFenced {
        return Vec::new();
    }
    local
        .iter()
        .filter(|vm| vm.backend == "ceph")
        .map(|vm| HostAction::Stop(vm.name.clone()))
        .collect()
}

pub fn isolation_due(last_ok: Option<Instant>, now: Instant, timeout: Duration) -> bool {
    match last_ok {
        Some(ok) => now.saturating_duration_since(ok) >= timeout,
        None => false,
    }
}

pub fn blocklist_contains_host(listing: &str, hosts: &[String]) -> bool {
    let hosts: Vec<&str> = hosts
        .iter()
        .map(|h| h.trim())
        .filter(|h| !h.is_empty())
        .collect();
    listing.lines().any(|line| {
        let line = line.trim();
        hosts.iter().any(|host| line.contains(host))
    })
}

pub fn monitor_actions(
    monitors: &[(String, i64)],
    heartbeats: &[(String, i64)],
    now: i64,
    last_restart: &[(String, i64)],
) -> Vec<HostAction> {
    let mut actions = Vec::new();
    for (name, after) in monitors {
        let hb = heartbeats
            .iter()
            .find(|(vm, _)| vm == name)
            .map(|(_, ts)| *ts)
            .unwrap_or(0);
        let restarted = last_restart
            .iter()
            .find(|(vm, _)| vm == name)
            .map(|(_, ts)| *ts)
            .unwrap_or(0);
        if crate_monitor_due(now, hb, *after, restarted) {
            actions.push(HostAction::Restart(name.clone()));
        }
    }
    actions
}

fn crate_monitor_due(now: i64, hb: i64, after: i64, last_restart: i64) -> bool {
    if after < 30 || hb <= 0 {
        return false;
    }
    if last_restart > 0 && now - last_restart < after {
        return false;
    }
    now - hb >= after
}

pub fn read_guest_heartbeat(dir: &Path, vm: &str) -> i64 {
    let path = dir.join(vm);
    let Ok(text) = std::fs::read_to_string(path) else {
        return 0;
    };
    text.trim().parse().unwrap_or(0)
}

/// Apply host actions when systemd is running. Returns the actions it
/// attempted so the next heartbeat can report them. Missing systemd is a
/// no-op, which is what unit tests and a developer laptop need.
pub fn apply_host_actions(actions: &[HostAction]) -> Vec<String> {
    if !Path::new("/run/systemd/system").exists() {
        return Vec::new();
    }
    let mut reported = Vec::new();
    for action in actions {
        let (program_args, report): (Vec<String>, String) = match action {
            HostAction::Mask(name) => (
                vec![
                    "mask".into(),
                    "--now".into(),
                    format!("kcore-vm-{name}.service"),
                ],
                format!("placement-mask:{name}"),
            ),
            HostAction::Unmask(name) => (
                vec!["unmask".into(), format!("kcore-vm-{name}.service")],
                String::new(),
            ),
            HostAction::StartTarget => (
                vec!["start".into(), "kcore-placement-confirmed.target".into()],
                String::new(),
            ),
            HostAction::Stop(name) => (
                vec!["stop".into(), format!("kcore-vm-{name}.service")],
                format!("isolation-poweroff:{name}"),
            ),
            HostAction::Restart(name) => (
                vec!["restart".into(), format!("kcore-vm-{name}.service")],
                format!("monitor-restart:{name}"),
            ),
        };
        let _ = std::process::Command::new("systemctl")
            .args(&program_args)
            .status();
        if !report.is_empty() {
            reported.push(report);
        }
    }
    reported
}

/// Shared with the heartbeat loop. The supervisor reads the assignment the
/// controller confirmed; the heartbeat sends `pending_reports` next time.
#[derive(Debug, Default)]
pub struct HaShared {
    pub pending_reports: Vec<String>,
    pub assigned: Vec<String>,
    pub confirmed: bool,
    pub monitors: Vec<(String, i64)>,
    pub last_ok: Option<Instant>,
    pub last_restart: Vec<(String, i64)>,
}

pub fn spawn_ha_supervisor(
    shared: std::sync::Arc<std::sync::Mutex<HaShared>>,
    ha: crate::config::NodeHaConfig,
) {
    tokio::spawn(async move {
        let started = Instant::now();
        let mut placement_done = false;
        let mut last_assigned: Vec<String> = Vec::new();
        loop {
            tokio::time::sleep(Duration::from_secs(5)).await;
            let (assigned, confirmed, monitors, last_ok, mut last_restart) = {
                let guard = shared.lock().unwrap_or_else(|e| e.into_inner());
                (
                    guard.assigned.clone(),
                    guard.confirmed,
                    guard.monitors.clone(),
                    guard.last_ok,
                    guard.last_restart.clone(),
                )
            };
            let local = std::fs::read_to_string(&ha.placement_file)
                .map(|text| parse_placement_file(&text))
                .unwrap_or_default();
            let now = Instant::now();
            let placement_due = !placement_done || assigned != last_assigned;
            let isolate =
                isolation_due(last_ok, now, Duration::from_secs(ha.isolation_timeout_secs));
            let storage = if isolate && local.iter().any(|vm| vm.backend == "ceph") {
                storage_view(&local_host_addrs())
            } else {
                StorageView::ReachableNotFenced
            };
            let heartbeats: Vec<(String, i64)> = local
                .iter()
                .map(|vm| {
                    (
                        vm.name.clone(),
                        read_guest_heartbeat(Path::new(&ha.guest_heartbeat_dir), &vm.name),
                    )
                })
                .collect();
            let now_unix = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| i64::try_from(d.as_secs()).unwrap_or(0))
                .unwrap_or(0);
            let actions = supervisor_actions(
                &local,
                &assigned,
                confirmed,
                started,
                now,
                Duration::from_secs(ha.placement_timeout_secs),
                placement_due,
                last_ok,
                Duration::from_secs(ha.isolation_timeout_secs),
                parse_isolation_response(&ha.isolation_response),
                storage,
                &monitors,
                &heartbeats,
                &last_restart,
                now_unix,
            );
            if actions
                .iter()
                .any(|action| matches!(action, HostAction::StartTarget))
            {
                placement_done = true;
                last_assigned = assigned;
            }
            let reports = apply_host_actions(&actions);
            if Path::new("/run/systemd/system").exists() {
                for action in &actions {
                    if let HostAction::Restart(name) = action {
                        if let Some(slot) = last_restart.iter_mut().find(|(vm, _)| vm == name) {
                            slot.1 = now_unix;
                        } else {
                            last_restart.push((name.clone(), now_unix));
                        }
                    }
                }
            }
            let mut guard = shared.lock().unwrap_or_else(|e| e.into_inner());
            guard.pending_reports.extend(reports);
            guard.last_restart = last_restart;
        }
    });
}

#[allow(clippy::too_many_arguments)]
pub fn supervisor_actions(
    local: &[LocalVm],
    assigned: &[String],
    confirmed: bool,
    started: Instant,
    now: Instant,
    placement_timeout: Duration,
    placement_due: bool,
    last_ok: Option<Instant>,
    isolation_timeout: Duration,
    isolation: IsolationResponse,
    storage: StorageView,
    monitors: &[(String, i64)],
    heartbeats: &[(String, i64)],
    last_restart: &[(String, i64)],
    now_unix: i64,
) -> Vec<HostAction> {
    let mut actions = Vec::new();
    if placement_due {
        let timed_out = now.saturating_duration_since(started) >= placement_timeout;
        actions.extend(placement_actions(local, assigned, confirmed, timed_out));
    }
    if isolation_due(last_ok, now, isolation_timeout) {
        actions.extend(isolation_actions(local, storage, isolation));
    }
    actions.extend(monitor_actions(
        monitors,
        heartbeats,
        now_unix,
        last_restart,
    ));
    actions
}

fn local_host_addrs() -> Vec<String> {
    let Ok(out) = std::process::Command::new("ip")
        .args(["-o", "addr", "show"])
        .output()
    else {
        return Vec::new();
    };
    if !out.status.success() {
        return Vec::new();
    }
    let text = String::from_utf8_lossy(&out.stdout);
    let mut addrs = Vec::new();
    for line in text.lines() {
        let mut toks = line.split_whitespace();
        while let Some(tok) = toks.next() {
            if tok == "inet" || tok == "inet6" {
                if let Some(addr) = toks.next() {
                    let ip = addr.split('/').next().unwrap_or("");
                    if !ip.is_empty() && ip != "127.0.0.1" && ip != "::1" {
                        addrs.push(ip.to_string());
                    }
                }
            }
        }
    }
    addrs.sort();
    addrs.dedup();
    addrs
}

fn storage_view(hosts: &[String]) -> StorageView {
    let Ok(out) = std::process::Command::new("timeout")
        .args(["5", "ceph", "osd", "blocklist", "ls"])
        .output()
    else {
        return StorageView::Unreachable;
    };
    if !out.status.success() {
        return StorageView::Unreachable;
    }
    let listing = String::from_utf8_lossy(&out.stdout);
    if blocklist_contains_host(&listing, hosts) {
        StorageView::ReachableFenced
    } else {
        StorageView::ReachableNotFenced
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn vm(name: &str, backend: &str) -> LocalVm {
        LocalVm {
            name: name.into(),
            backend: backend.into(),
        }
    }

    #[test]
    fn placement_file_parses_name_and_backend() {
        let vms = parse_placement_file("web ceph\n# comment\n\ndb zfs\nlegacy\n");
        assert_eq!(
            vms,
            vec![
                vm("web", "ceph"),
                vm("db", "zfs"),
                vm("legacy", "filesystem")
            ]
        );
    }

    #[test]
    fn an_unconfirmed_controller_holds_units_until_timeout() {
        let local = vec![vm("web", "ceph"), vm("db", "zfs")];
        assert!(placement_actions(&local, &[], false, false).is_empty());
        let timed = placement_actions(&local, &[], false, true);
        assert!(timed.contains(&HostAction::Mask("web".into())));
        assert!(timed.contains(&HostAction::Unmask("db".into())));
        assert!(timed.contains(&HostAction::StartTarget));
    }

    #[test]
    fn a_moved_ceph_vm_is_masked_and_a_local_vm_starts() {
        let local = vec![vm("web", "ceph"), vm("db", "lvm")];
        let actions = placement_actions(&local, &["db".into()], true, false);
        assert_eq!(
            actions,
            vec![
                HostAction::Mask("web".into()),
                HostAction::Unmask("db".into()),
                HostAction::StartTarget,
            ]
        );
    }

    #[test]
    fn isolation_stops_ceph_only_when_storage_is_fenced_or_gone() {
        let local = vec![vm("web", "ceph"), vm("db", "zfs")];
        assert!(isolation_actions(
            &local,
            StorageView::ReachableNotFenced,
            IsolationResponse::PowerOff
        )
        .is_empty());
        assert!(isolation_actions(
            &local,
            StorageView::ReachableFenced,
            IsolationResponse::LeaveRunning
        )
        .is_empty());
        assert_eq!(
            isolation_actions(
                &local,
                StorageView::Unreachable,
                IsolationResponse::PowerOff
            ),
            vec![HostAction::Stop("web".into())]
        );
    }

    #[test]
    fn blocklist_matches_the_host_address() {
        let listing = "192.0.2.10:6801/17\n10.0.0.8:0/1\n";
        assert!(blocklist_contains_host(listing, &["192.0.2.10".into()]));
        assert!(!blocklist_contains_host(listing, &["192.0.2.11".into()]));
    }

    #[test]
    fn guest_monitor_restarts_once_per_interval() {
        let actions = monitor_actions(&[("web".into(), 120)], &[("web".into(), 800)], 1_000, &[]);
        assert_eq!(actions, vec![HostAction::Restart("web".into())]);
        assert!(monitor_actions(
            &[("web".into(), 120)],
            &[("web".into(), 800)],
            1_000,
            &[("web".into(), 950)],
        )
        .is_empty());
        assert!(monitor_actions(&[("web".into(), 120)], &[], 1_000, &[]).is_empty());
    }

    #[test]
    fn supervisor_holds_placement_then_masks_ceph_and_isolates() {
        let local = vec![vm("web", "ceph"), vm("db", "zfs")];
        let start = Instant::now();
        let waiting = supervisor_actions(
            &local,
            &[],
            false,
            start,
            start,
            Duration::from_secs(300),
            true,
            Some(start),
            Duration::from_secs(60),
            IsolationResponse::PowerOff,
            StorageView::ReachableNotFenced,
            &[],
            &[],
            &[],
            0,
        );
        assert!(waiting.is_empty());
        let timed = supervisor_actions(
            &local,
            &[],
            false,
            start,
            start + Duration::from_secs(300),
            Duration::from_secs(300),
            true,
            Some(start),
            Duration::from_secs(60),
            IsolationResponse::PowerOff,
            StorageView::Unreachable,
            &[],
            &[],
            &[],
            0,
        );
        assert!(timed.contains(&HostAction::Mask("web".into())));
        assert!(timed.contains(&HostAction::Stop("web".into())));
        assert!(!timed
            .iter()
            .any(|action| matches!(action, HostAction::Stop(name) if name == "db")));
    }

    fn isolation_waits_for_a_successful_heartbeat() {
        let start = Instant::now();
        assert!(!isolation_due(None, start, Duration::from_secs(60)));
        assert!(isolation_due(
            Some(start),
            start + Duration::from_secs(60),
            Duration::from_secs(60)
        ));
        assert!(!isolation_due(
            Some(start),
            start + Duration::from_secs(10),
            Duration::from_secs(60)
        ));
    }
}
