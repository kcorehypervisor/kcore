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

/// Placement runs on the first pass, when the controller's assignment
/// changes, when the fence list changes, and when `vm-placement` itself
/// changes. A moved guest's line often appears only after `nixos-rebuild`
/// writes the file, which is later than the assignment heartbeat. A
/// reinstall never writes that line, so the fence list is the other signal.
pub fn placement_cycle_due(
    done: bool,
    assigned: &[String],
    last_assigned: &[String],
    fence_vms: &[String],
    last_fence: &[String],
    placement_text: &str,
    last_placement: &str,
) -> bool {
    !done
        || assigned != last_assigned
        || fence_vms != last_fence
        || placement_text != last_placement
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

/// `systemctl is-active` prints one of these when the unit is not running
/// and is not on its way up. Anything else (including an error line) is not
/// a stopped guest.
pub fn unit_state_is_down(state: &str) -> bool {
    matches!(state.trim(), "inactive" | "failed" | "deactivating")
}

/// Pair each guest with the matching `systemctl is-active` line.
pub fn zip_unit_states(
    names: &[String],
    stdout: &str,
) -> std::collections::HashMap<String, String> {
    names
        .iter()
        .zip(stdout.lines())
        .filter_map(|(name, line)| {
            let state = line.trim();
            if state.is_empty() {
                None
            } else {
                Some((name.clone(), state.to_string()))
            }
        })
        .collect()
}

/// Placement-file guests that have no VMM socket and whose unit is down.
///
/// `unit_states` is `None` when `systemctl` could not be executed. Nothing
/// is returned then: a missing `systemctl` must not mark every guest stopped.
/// A name with no state, or a state that is `active`, `activating`, or
/// `reloading`, is left to the socket list. A guest that still has a socket
/// is already reported from that socket, even when systemd says inactive.
pub fn stopped_guests_without_socket(
    placement: &[LocalVm],
    socket_names: &[String],
    unit_states: Option<&std::collections::HashMap<String, String>>,
) -> Vec<String> {
    let Some(states) = unit_states else {
        return Vec::new();
    };
    let mut stopped = Vec::new();
    for vm in placement {
        if socket_names.iter().any(|name| name == &vm.name) || stopped.contains(&vm.name) {
            continue;
        }
        let Some(state) = states.get(&vm.name) else {
            continue;
        };
        if unit_state_is_down(state) {
            stopped.push(vm.name.clone());
        }
    }
    stopped
}

/// What to do with the units in the booted configuration.
///
/// `confirmed` false means the controller is too old to say, so every unit
/// may start. After `timed_out`, Ceph units that were never confirmed are
/// masked and the target is started so local disks boot.
///
/// `fence_vms` are guests the controller moved away and still blocklists
/// this host for. They are masked even when `vm-placement` no longer names
/// them. A unit that is not installed counts as masked. A guest the
/// controller still assigns here is left alone.
pub fn placement_actions(
    local: &[LocalVm],
    assigned: &[String],
    fence_vms: &[String],
    confirmed: bool,
    timed_out: bool,
) -> Vec<HostAction> {
    if !confirmed && !timed_out {
        return Vec::new();
    }
    let mut actions = Vec::new();
    for vm in local {
        // An old controller never confirms, so after the wait local disks
        // boot and Ceph stays masked: that disk may already be running
        // somewhere else. A confirmed assignment masks only the Ceph guests
        // the controller has moved away.
        let owned = if confirmed {
            assigned.iter().any(|id| id == &vm.name)
        } else {
            vm.backend != "ceph"
        };
        if vm.backend == "ceph" && !owned {
            actions.push(HostAction::Mask(vm.name.clone()));
        } else {
            actions.push(HostAction::Unmask(vm.name.clone()));
        }
    }
    if confirmed {
        for name in fence_vms {
            let name = name.trim();
            if name.is_empty() {
                continue;
            }
            let already = actions.iter().any(|action| {
                matches!(action, HostAction::Mask(have) | HostAction::Unmask(have) if have == name)
            });
            let owned = assigned.iter().any(|id| id == name);
            if !already && !owned {
                actions.push(HostAction::Mask(name.to_string()));
            }
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

/// A single missed `ceph osd blocklist` query is not proof that storage is
/// gone. Power-off waits for a second unreachable or fenced reading so a
/// slow OSD op during a management cut does not stop the guest.
pub fn confirmed_storage_view(streak: u32, view: StorageView) -> (u32, StorageView) {
    match view {
        StorageView::ReachableNotFenced => (0, view),
        other => {
            let streak = streak.saturating_add(1);
            if streak >= 2 {
                (streak, other)
            } else {
                (streak, StorageView::ReachableNotFenced)
            }
        }
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

/// A guest tools file is the heartbeat when the guest has written one.
/// Until then, the last time the VMM answered `vm.info` is the heartbeat,
/// so a wedged hypervisor is restarted and a healthy guest without tools is not.
pub fn effective_monitor_heartbeat(guest_unix: i64, vmm_unix: i64) -> i64 {
    if guest_unix > 0 {
        guest_unix
    } else {
        vmm_unix
    }
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

/// What the next heartbeat should tell the controller about this action.
pub fn action_report(action: &HostAction) -> Option<String> {
    match action {
        HostAction::Mask(name) => Some(format!("placement-mask:{name}")),
        HostAction::Unmask(_) => None,
        HostAction::StartTarget => Some("placement-settled".into()),
        HostAction::Stop(name) => Some(format!("isolation-poweroff:{name}")),
        HostAction::Restart(name) => Some(format!("monitor-restart:{name}")),
    }
}

/// Runtime drop-in that stops a NixOS VM unit from mapping its disk.
///
/// `systemctl mask` refuses to replace the store symlink NixOS puts in
/// `/etc/systemd/system`, and a `/run` mask loses to that `/etc` unit.
/// Clearing `ExecStartPre` matters: that hook maps the RBD, so a unit that
/// only loses `ExecStart` can still open the image the guest now runs elsewhere.
pub fn placement_mask_dropin() -> &'static str {
    "[Service]\n\
     Restart=no\n\
     ExecStartPre=\n\
     ExecStart=\n\
     ExecStop=\n\
     ExecStopPost=\n\
     ExecStart=/run/current-system/sw/bin/true\n"
}

pub(crate) fn vm_unit(name: &str) -> String {
    format!("kcore-vm-{name}.service")
}

fn systemctl_ok(args: &[&str]) -> bool {
    std::process::Command::new("systemctl")
        .args(args)
        .status()
        .map(|status| status.success())
        .unwrap_or(false)
}

/// `systemctl is-active` exits 0 while the unit is active or activating.
fn unit_is_inactive(unit: &str) -> bool {
    !systemctl_ok(&["is-active", "--quiet", unit])
}

/// Install the runtime mask and stop the unit. Reports success only when
/// the unit is not running afterwards.
fn apply_mask(name: &str) -> bool {
    let unit = vm_unit(name);
    let dir = format!("/run/systemd/system/{unit}.d");
    let path = format!("{dir}/placement-mask.conf");
    if std::fs::create_dir_all(&dir).is_err()
        || std::fs::write(&path, placement_mask_dropin()).is_err()
    {
        return false;
    }
    if !systemctl_ok(&["daemon-reload"]) {
        return false;
    }
    let _ = systemctl_ok(&["stop", &unit]);
    unit_is_inactive(&unit)
}

fn apply_unmask(name: &str) -> bool {
    let unit = vm_unit(name);
    let path = format!("/run/systemd/system/{unit}.d/placement-mask.conf");
    let masked_at_unix = std::fs::metadata(&path).ok().and_then(file_mtime_unix);
    // Read the unit before the drop-in is gone. `true` has usually already
    // exited, and the timestamps are the only record that this start ran
    // the mask instead of the guest.
    let snapshot = masked_at_unix.map(|_| unit_runtime_snapshot(&unit));
    let was_masked = std::fs::remove_file(&path).is_ok();
    let _ = systemctl_ok(&["daemon-reload"]);
    let _ = systemctl_ok(&["unmask", "--runtime", &unit]);
    let restart = was_masked
        && snapshot.as_ref().is_some_and(|snapshot| {
            masked_at_unix.is_some_and(|masked_at_unix| {
                masked_unit_needs_restart(
                    snapshot.active,
                    snapshot.failed,
                    snapshot.main_is_dummy,
                    snapshot.exec_start_unix,
                    snapshot.exec_exit_unix,
                    masked_at_unix,
                )
            })
        });
    if restart {
        let _ = systemctl_ok(&["restart", &unit]);
    }
    true
}

struct UnitRuntime {
    active: bool,
    failed: bool,
    main_is_dummy: bool,
    exec_start_unix: Option<u64>,
    exec_exit_unix: Option<u64>,
}

fn file_mtime_unix(meta: std::fs::Metadata) -> Option<u64> {
    meta.modified()
        .ok()
        .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|elapsed| elapsed.as_secs())
}

fn unit_runtime_snapshot(unit: &str) -> UnitRuntime {
    let props = systemctl_show(unit);
    let state = props.get("ActiveState").map(String::as_str).unwrap_or("");
    let pid = props
        .get("MainPID")
        .and_then(|raw| raw.parse::<u32>().ok())
        .unwrap_or(0);
    UnitRuntime {
        active: state == "active" || state == "activating" || state == "reloading",
        failed: state == "failed",
        main_is_dummy: pid > 0 && process_is_dummy_true(pid),
        exec_start_unix: props
            .get("ExecMainStartTimestamp")
            .and_then(|raw| parse_systemd_unix_timestamp(raw)),
        exec_exit_unix: props
            .get("ExecMainExitTimestamp")
            .and_then(|raw| parse_systemd_unix_timestamp(raw)),
    }
}

fn systemctl_show(unit: &str) -> std::collections::HashMap<String, String> {
    let output = std::process::Command::new("systemctl")
        .args([
            "show",
            "--timestamp=unix",
            "--no-pager",
            "-p",
            "ActiveState",
            "-p",
            "MainPID",
            "-p",
            "ExecMainStartTimestamp",
            "-p",
            "ExecMainExitTimestamp",
            unit,
        ])
        .output();
    let mut map = std::collections::HashMap::new();
    let Ok(output) = output else {
        return map;
    };
    for line in String::from_utf8_lossy(&output.stdout).lines() {
        if let Some((key, value)) = line.split_once('=') {
            map.insert(key.to_string(), value.to_string());
        }
    }
    map
}

fn process_is_dummy_true(pid: u32) -> bool {
    let Ok(cmdline) = std::fs::read(format!("/proc/{pid}/cmdline")) else {
        return false;
    };
    let first = cmdline.split(|byte| *byte == 0).next().unwrap_or_default();
    let Ok(name) = std::str::from_utf8(first) else {
        return false;
    };
    name == "true" || name.ends_with("/true")
}

/// A start or stop that happened while the placement mask was installed has
/// to be redone with the real unit. `true` exits 0, so that start leaves the
/// unit inactive rather than failed. A guest the operator stopped before
/// this mask was written stays stopped.
pub fn masked_unit_needs_restart(
    active: bool,
    failed: bool,
    main_is_dummy: bool,
    exec_start_unix: Option<u64>,
    exec_exit_unix: Option<u64>,
    masked_at_unix: u64,
) -> bool {
    if failed || (active && main_is_dummy) {
        return true;
    }
    if active {
        return false;
    }
    let under_mask = |stamp: Option<u64>| stamp.is_some_and(|t| t >= masked_at_unix);
    under_mask(exec_start_unix) || under_mask(exec_exit_unix)
}

/// systemd `--timestamp=unix` prints `@<seconds>`, sometimes with a fraction.
pub fn parse_systemd_unix_timestamp(raw: &str) -> Option<u64> {
    let text = raw.trim();
    if text.is_empty() || text.eq_ignore_ascii_case("n/a") {
        return None;
    }
    let text = text.strip_prefix('@').unwrap_or(text);
    let text = text.split('.').next().unwrap_or(text);
    let secs: u64 = text.parse().ok()?;
    if secs == 0 {
        None
    } else {
        Some(secs)
    }
}

fn systemctl_args(action: &HostAction) -> Option<Vec<String>> {
    match action {
        HostAction::Mask(_) | HostAction::Unmask(_) => None,
        HostAction::StartTarget => Some(vec![
            "start".into(),
            "kcore-placement-confirmed.target".into(),
        ]),
        HostAction::Stop(name) => Some(vec!["stop".into(), vm_unit(name)]),
        HostAction::Restart(name) => Some(vec!["restart".into(), vm_unit(name)]),
    }
}

/// Apply host actions when systemd is running. A mask is reported only after
/// the unit is inactive: telling the controller the guest is masked while it
/// is still running releases the blocklist under a second writer.
///
/// `kcore-placement-confirmed.target` is reported even when the unit is
/// absent. An older module has no such target, and the fence hold still has
/// to release once the masks themselves succeeded.
pub fn apply_host_actions(actions: &[HostAction]) -> Vec<String> {
    if !Path::new("/run/systemd/system").exists() {
        return Vec::new();
    }
    let mut reported = Vec::new();
    for action in actions {
        let ok = match action {
            HostAction::Mask(name) => apply_mask(name),
            HostAction::Unmask(name) => apply_unmask(name),
            HostAction::StartTarget => {
                if let Some(args) = systemctl_args(action) {
                    let _ = systemctl_ok(&args.iter().map(String::as_str).collect::<Vec<_>>());
                }
                true
            }
            other => systemctl_args(other)
                .map(|args| systemctl_ok(&args.iter().map(String::as_str).collect::<Vec<_>>()))
                .unwrap_or(false),
        };
        if ok {
            if let Some(report) = action_report(action) {
                reported.push(report);
            }
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
    /// Guests the controller still fences this host for. Masked even when
    /// they are absent from the local placement file.
    pub fence_vms: Vec<String>,
    pub confirmed: bool,
    pub monitors: Vec<(String, i64)>,
    pub last_ok: Option<Instant>,
    pub last_restart: Vec<(String, i64)>,
}

async fn monitor_heartbeats(
    vmm: &crate::vmm::Client,
    monitors: &[(String, i64)],
    guest_dir: &str,
    vmm_seen: &mut Vec<(String, i64)>,
) -> Vec<(String, i64)> {
    if monitors.is_empty() {
        return Vec::new();
    }
    let now_unix = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| i64::try_from(elapsed.as_secs()).unwrap_or(0))
        .unwrap_or(0);
    let mut heartbeats = Vec::new();
    for (name, _) in monitors {
        // A stopped unit is an operator power-off. Monitoring must not turn
        // that guest back on. A wedged running VMM stays active and then
        // stops answering, which is the case that restarts.
        if unit_is_inactive(&vm_unit(name)) {
            continue;
        }
        let guest = read_guest_heartbeat(Path::new(guest_dir), name);
        let mut vmm_unix = vmm_seen
            .iter()
            .find(|(vm, _)| vm == name)
            .map(|(_, ts)| *ts)
            .unwrap_or(0);
        if let Some(info) = vmm.get_vm_info(name).await {
            if info.state.eq_ignore_ascii_case("running")
                || info.state.eq_ignore_ascii_case("paused")
            {
                vmm_unix = now_unix;
                if let Some(slot) = vmm_seen.iter_mut().find(|(vm, _)| vm == name) {
                    slot.1 = now_unix;
                } else {
                    vmm_seen.push((name.clone(), now_unix));
                }
            }
        }
        heartbeats.push((name.clone(), effective_monitor_heartbeat(guest, vmm_unix)));
    }
    heartbeats
}

pub fn spawn_ha_supervisor(
    shared: std::sync::Arc<std::sync::Mutex<HaShared>>,
    ha: crate::config::NodeHaConfig,
    socket_dir: String,
) {
    tokio::spawn(async move {
        let started = Instant::now();
        let mut placement_done = false;
        let mut last_assigned: Vec<String> = Vec::new();
        let mut last_fence: Vec<String> = Vec::new();
        let mut last_placement = String::new();
        let mut storage_streak = 0u32;
        let mut vmm_seen: Vec<(String, i64)> = Vec::new();
        let vmm = crate::vmm::Client::new(&socket_dir);
        loop {
            tokio::time::sleep(Duration::from_secs(5)).await;
            let (assigned, fence_vms, confirmed, monitors, last_ok, mut last_restart) = {
                let guard = shared.lock().unwrap_or_else(|e| e.into_inner());
                (
                    guard.assigned.clone(),
                    guard.fence_vms.clone(),
                    guard.confirmed,
                    guard.monitors.clone(),
                    guard.last_ok,
                    guard.last_restart.clone(),
                )
            };
            let placement_text = std::fs::read_to_string(&ha.placement_file).unwrap_or_default();
            let local = parse_placement_file(&placement_text);
            let now = Instant::now();
            let placement_due = placement_cycle_due(
                placement_done,
                &assigned,
                &last_assigned,
                &fence_vms,
                &last_fence,
                &placement_text,
                &last_placement,
            );
            let isolate =
                isolation_due(last_ok, now, Duration::from_secs(ha.isolation_timeout_secs));
            let storage = if isolate && local.iter().any(|vm| vm.backend == "ceph") {
                let view = storage_view(&local_host_addrs());
                let (streak, view) = confirmed_storage_view(storage_streak, view);
                storage_streak = streak;
                view
            } else {
                storage_streak = 0;
                StorageView::ReachableNotFenced
            };
            let heartbeats =
                monitor_heartbeats(&vmm, &monitors, &ha.guest_heartbeat_dir, &mut vmm_seen).await;
            let now_unix = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| i64::try_from(d.as_secs()).unwrap_or(0))
                .unwrap_or(0);
            let actions = supervisor_actions(
                &local,
                &assigned,
                &fence_vms,
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
            let reports = apply_host_actions(&actions);
            // A mask that did not land has to be tried again. Marking the
            // cycle done first left a moved guest unmasked, and the fence
            // hold never cleared.
            let mask_failed = actions.iter().any(|action| {
                let HostAction::Mask(name) = action else {
                    return false;
                };
                !reports
                    .iter()
                    .any(|report| report == &format!("placement-mask:{name}"))
            });
            if actions
                .iter()
                .any(|action| matches!(action, HostAction::StartTarget))
                && !mask_failed
            {
                placement_done = true;
                last_assigned = assigned;
                last_fence = fence_vms;
                last_placement = placement_text;
            }
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
    fence_vms: &[String],
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
        actions.extend(placement_actions(
            local, assigned, fence_vms, confirmed, timed_out,
        ));
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
        .args(["15", "ceph", "osd", "blocklist", "ls"])
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
    fn placement_runs_again_when_the_file_or_the_assignment_changes() {
        let assigned = vec!["labtest-9".to_string()];
        let text = "labtest-9 ceph\n";
        assert!(placement_cycle_due(
            false,
            &assigned,
            &[],
            &[],
            &[],
            text,
            ""
        ));
        assert!(!placement_cycle_due(
            true,
            &assigned,
            &assigned,
            &[],
            &[],
            text,
            text
        ));
        assert!(
            placement_cycle_due(true, &assigned, &assigned, &[], &[], text, ""),
            "a placement file written after the first pass still has to be applied"
        );
        assert!(placement_cycle_due(
            true,
            &assigned,
            &[],
            &[],
            &[],
            text,
            text
        ));
        let fence = vec!["labtest-9".to_string()];
        assert!(
            placement_cycle_due(true, &assigned, &assigned, &fence, &[], text, text),
            "a fence list that arrives after the first pass still has to be applied"
        );
        assert!(!placement_cycle_due(
            true, &assigned, &assigned, &fence, &fence, text, text
        ));
    }

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
    fn a_down_unit_with_no_socket_is_reported_stopped() {
        let placement = vec![
            vm("win10-01", "lvm"),
            vm("ha-live", "ceph"),
            vm("starting", "lvm"),
            vm("reloading", "filesystem"),
            vm("win10-01", "lvm"),
            vm("leaving", "ceph"),
            vm("broken", "lvm"),
            vm("mystery", "lvm"),
        ];
        let sockets = vec!["ha-live".to_string()];
        let mut states = zip_unit_states(
            &[
                "win10-01".into(),
                "ha-live".into(),
                "starting".into(),
                "reloading".into(),
                "leaving".into(),
            ],
            "inactive\nactive\nactivating\nreloading\ndeactivating\n",
        );
        states.insert("broken".into(), "failed".into());
        states.insert("mystery".into(), "Failed to connect to bus".into());

        assert_eq!(
            stopped_guests_without_socket(&placement, &sockets, Some(&states)),
            vec![
                "win10-01".to_string(),
                "leaving".to_string(),
                "broken".to_string()
            ]
        );
        assert!(stopped_guests_without_socket(&placement, &sockets, None).is_empty());
        assert!(unit_state_is_down("inactive"));
        assert!(!unit_state_is_down("activating"));
        assert_eq!(vm_unit("win10-01"), "kcore-vm-win10-01.service");
    }

    #[test]
    fn an_unconfirmed_controller_holds_units_until_timeout() {
        let local = vec![vm("web", "ceph"), vm("db", "zfs")];
        assert!(placement_actions(&local, &[], &[], false, false).is_empty());
        let timed = placement_actions(&local, &[], &[], false, true);
        assert!(timed.contains(&HostAction::Mask("web".into())));
        assert!(timed.contains(&HostAction::Unmask("db".into())));
        assert!(timed.contains(&HostAction::StartTarget));
    }

    #[test]
    fn systemd_unix_timestamp_accepts_the_at_prefix() {
        assert_eq!(
            parse_systemd_unix_timestamp("@1792599110"),
            Some(1_792_599_110)
        );
        assert_eq!(
            parse_systemd_unix_timestamp("1792599110"),
            Some(1_792_599_110)
        );
        assert_eq!(
            parse_systemd_unix_timestamp("@1792599110.5"),
            Some(1_792_599_110)
        );
        assert_eq!(parse_systemd_unix_timestamp(""), None);
        assert_eq!(parse_systemd_unix_timestamp("n/a"), None);
        assert_eq!(parse_systemd_unix_timestamp("@0"), None);
    }

    #[test]
    fn a_dummy_start_under_the_mask_is_replaced_and_a_stopped_guest_is_not() {
        let masked_at = 1_000;
        assert!(masked_unit_needs_restart(
            false,
            false,
            false,
            Some(1_005),
            Some(1_005),
            masked_at
        ));
        assert!(masked_unit_needs_restart(
            false, true, false, None, None, masked_at
        ));
        assert!(masked_unit_needs_restart(
            true,
            false,
            true,
            Some(1_005),
            None,
            masked_at
        ));
        assert!(!masked_unit_needs_restart(
            true,
            false,
            false,
            Some(900),
            None,
            masked_at
        ));
        assert!(!masked_unit_needs_restart(
            false,
            false,
            false,
            Some(800),
            Some(900),
            masked_at
        ));
        assert!(!masked_unit_needs_restart(
            false, false, false, None, None, masked_at
        ));
        assert!(masked_unit_needs_restart(
            false,
            false,
            false,
            Some(900),
            Some(1_010),
            masked_at
        ));
    }

    #[test]
    fn placement_mask_dropin_clears_the_rbd_map_hook() {
        let dropin = placement_mask_dropin();
        assert!(dropin.contains("Restart=no\n"));
        assert!(dropin.contains("ExecStartPre=\n"));
        assert!(dropin.contains("ExecStart=\n"));
        assert!(dropin.contains("ExecStopPost=\n"));
        assert!(dropin.contains("ExecStart=/run/current-system/sw/bin/true\n"));
    }

    #[test]
    fn placement_reports_the_mask_and_that_placement_settled() {
        let actions = [
            HostAction::Mask("web".into()),
            HostAction::Unmask("db".into()),
            HostAction::StartTarget,
        ];
        let reports: Vec<String> = actions.iter().filter_map(action_report).collect();
        assert_eq!(
            reports,
            vec![
                "placement-mask:web".to_string(),
                "placement-settled".to_string()
            ]
        );
    }

    #[test]
    fn a_moved_ceph_vm_is_masked_and_a_local_vm_starts() {
        let local = vec![vm("web", "ceph"), vm("db", "lvm")];
        let actions = placement_actions(&local, &["db".into()], &[], true, false);
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
    fn a_fenced_vm_missing_from_the_placement_file_is_still_masked() {
        let local = vec![vm("db", "lvm")];
        let actions = placement_actions(
            &local,
            &["db".into()],
            &["labtest-9".into(), " ".into()],
            true,
            false,
        );
        assert_eq!(
            actions,
            vec![
                HostAction::Unmask("db".into()),
                HostAction::Mask("labtest-9".into()),
                HostAction::StartTarget,
            ]
        );
    }

    #[test]
    fn a_fenced_vm_that_still_belongs_here_is_not_masked() {
        let actions = placement_actions(
            &[],
            &["labtest-9".into()],
            &["labtest-9".into()],
            true,
            false,
        );
        assert_eq!(actions, vec![HostAction::StartTarget]);
    }

    #[test]
    fn an_unconfirmed_controller_does_not_apply_a_stale_fence_list() {
        let actions = placement_actions(&[], &[], &["labtest-9".into()], false, true);
        assert_eq!(actions, vec![HostAction::StartTarget]);
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
    fn vmm_heartbeat_keeps_a_guest_without_tools_and_a_stale_file_still_restarts() {
        let live = effective_monitor_heartbeat(0, 900);
        assert!(
            monitor_actions(&[("web".into(), 120)], &[("web".into(), live)], 1_000, &[]).is_empty()
        );
        let wedged = effective_monitor_heartbeat(0, 800);
        assert_eq!(
            monitor_actions(
                &[("web".into(), 120)],
                &[("web".into(), wedged)],
                1_000,
                &[]
            ),
            vec![HostAction::Restart("web".into())]
        );
        // The guest file wins once it exists, even if the VMM just answered.
        assert_eq!(effective_monitor_heartbeat(100, 990), 100);
    }

    #[test]
    fn supervisor_holds_placement_then_masks_ceph_and_isolates() {
        let local = vec![vm("web", "ceph"), vm("db", "zfs")];
        let start = Instant::now();
        let waiting = supervisor_actions(
            &local,
            &[],
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
            &["labtest-9".into()],
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
        assert!(!timed.contains(&HostAction::Mask("labtest-9".into())));
        assert!(timed.contains(&HostAction::Stop("web".into())));
        assert!(!timed
            .iter()
            .any(|action| matches!(action, HostAction::Stop(name) if name == "db")));
    }

    #[test]
    fn isolation_waits_for_a_successful_heartbeat() {
        let start = Instant::now();
        let (streak, quiet) = confirmed_storage_view(0, StorageView::Unreachable);
        assert_eq!(streak, 1);
        assert_eq!(quiet, StorageView::ReachableNotFenced);
        let (streak, stop) = confirmed_storage_view(streak, StorageView::Unreachable);
        assert_eq!(streak, 2);
        assert_eq!(stop, StorageView::Unreachable);
        let (streak, clear) = confirmed_storage_view(streak, StorageView::ReachableNotFenced);
        assert_eq!(streak, 0);
        assert_eq!(clear, StorageView::ReachableNotFenced);
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
