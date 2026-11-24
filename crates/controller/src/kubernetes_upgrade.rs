//! Rolling a cluster to a new Kubernetes version one node at a time.
//!
//! Control planes go first, then workers, because a kubelet must never be
//! newer than the API servers it talks to. A node that runs pods is drained
//! before its rebuild and uncordoned only after its kubelet reports the new
//! version. Each node's progress is stored, so a controller restart resumes
//! where the last one stopped.

use std::cmp::Ordering;

use crate::kubernetes_versions;

/// Status message prefix of a cluster whose upgrade stopped on a node. Health
/// promotion leaves such a cluster `degraded` until the operator re-applies.
pub const UPGRADE_FAILED: &str = "Kubernetes upgrade stopped";

pub const STATE_IDLE: &str = "";
pub const STATE_DRAINED: &str = "drained";
pub const STATE_APPLYING: &str = "applying";
pub const STATE_FAILED: &str = "failed";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApplyProgress {
    /// The controller stopped before it stored the apply id; push again.
    NotRecorded,
    /// The agent no longer knows the apply, for example after a restart.
    Unknown,
    Running,
    Succeeded,
    Failed,
}

/// A node rebuild the controller started and has not seen finish.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingApply {
    pub node_id: String,
    pub address: String,
    pub apply_id: String,
}

const APPLY_SUMMARY_MAX: usize = 400;

/// The line that explains a failed rebuild: the last Nix `error:` line, or
/// the last non-empty line. The agent stores the whole stderr.
pub fn apply_failure_summary(message: &str) -> String {
    let lines: Vec<&str> = message
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .collect();
    let line = lines
        .iter()
        .rev()
        .find_map(|l| l.find("error:").map(|at| &l[at..]))
        .or_else(|| lines.last().copied())
        .unwrap_or("no error output");
    if line.chars().count() <= APPLY_SUMMARY_MAX {
        return line.to_string();
    }
    let mut cut: String = line.chars().take(APPLY_SUMMARY_MAX).collect();
    cut.push('…');
    cut
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UpgradeMember {
    pub node_id: String,
    pub role: String,
    /// Version rendered into this node's Nix.
    pub version: String,
    pub state: String,
    pub cordoned: bool,
    pub apply: ApplyProgress,
    pub apply_message: String,
    /// Kubelet version the API reports for the node; empty when unregistered.
    pub kubelet_version: String,
    pub node_ready: bool,
    pub etcd_healthy: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UpgradeView {
    pub desired: String,
    pub api_ready: bool,
    pub members: Vec<UpgradeMember>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UpgradeStep {
    Wait { node_id: String, message: String },
    Drain { node_id: String },
    Apply { node_id: String },
    Uncordon { node_id: String },
    Finish { node_id: String },
    Fail { node_id: String, message: String },
    Done,
}

fn is_control_plane(member: &UpgradeMember) -> bool {
    member.role != "worker"
}

/// Control planes first, each group in node id order.
pub fn rollout_order(members: &[UpgradeMember]) -> Vec<&UpgradeMember> {
    let mut ordered: Vec<&UpgradeMember> = members.iter().collect();
    ordered.sort_by(|a, b| {
        is_control_plane(b)
            .cmp(&is_control_plane(a))
            .then_with(|| a.node_id.cmp(&b.node_id))
    });
    ordered
}

fn same_version(a: &str, b: &str) -> bool {
    kubernetes_versions::compare(a, b) == Ordering::Equal
}

pub fn member_is_done(member: &UpgradeMember, desired: &str) -> bool {
    member.state == STATE_IDLE && same_version(&member.version, desired)
}

/// `(done, total)` for a status message.
pub fn progress(view: &UpgradeView) -> (usize, usize) {
    let done = view
        .members
        .iter()
        .filter(|member| member_is_done(member, &view.desired))
        .count();
    (done, view.members.len())
}

const FAILURE_SUMMARY_CHARS: usize = 240;

/// The agent reports the whole nixos-rebuild output. The cluster message
/// keeps the last line, which names the step that failed.
fn failure_summary(output: &str) -> String {
    let Some(last) = output.lines().map(str::trim).rfind(|line| !line.is_empty()) else {
        return "nixos-rebuild failed".to_string();
    };
    let chars = last.chars().count();
    if chars <= FAILURE_SUMMARY_CHARS {
        return last.to_string();
    }
    let tail: String = last.chars().skip(chars - FAILURE_SUMMARY_CHARS).collect();
    format!("...{tail}")
}

pub fn next_upgrade_step(view: &UpgradeView) -> UpgradeStep {
    let desired = kubernetes_versions::effective(&view.desired);
    let control_planes: Vec<&UpgradeMember> = view
        .members
        .iter()
        .filter(|member| is_control_plane(member))
        .collect();

    for member in rollout_order(&view.members) {
        if member_is_done(member, &desired) {
            continue;
        }
        let node_id = member.node_id.clone();
        let wait = |message: String| UpgradeStep::Wait {
            node_id: node_id.clone(),
            message,
        };

        if member.state == STATE_APPLYING {
            match member.apply {
                ApplyProgress::Running => {
                    return wait(format!("rebuilding {node_id} with Kubernetes {desired}"));
                }
                ApplyProgress::Failed => {
                    let detail = failure_summary(&member.apply_message);
                    return UpgradeStep::Fail {
                        node_id,
                        message: format!("rebuild of {} failed: {detail}", member.node_id),
                    };
                }
                ApplyProgress::NotRecorded => return UpgradeStep::Apply { node_id },
                ApplyProgress::Succeeded | ApplyProgress::Unknown => {}
            }
            let kubelet = kubernetes_versions::normalize(&member.kubelet_version);
            if kubelet.is_empty() {
                return wait(format!("waiting for {node_id} to register with the API"));
            }
            if !same_version(&kubelet, &desired) {
                return wait(format!(
                    "waiting for {node_id} to report kubelet {desired} (reports {kubelet})"
                ));
            }
            if !member.node_ready {
                return wait(format!("waiting for {node_id} to become Ready"));
            }
            if is_control_plane(member) && !member.etcd_healthy {
                return wait(format!("waiting for etcd on {node_id} to be healthy"));
            }
            if is_control_plane(member) && !view.api_ready {
                return wait("waiting for the API server after the upgrade".to_string());
            }
            if member.cordoned {
                return UpgradeStep::Uncordon { node_id };
            }
            return UpgradeStep::Finish { node_id };
        }

        // The next node starts only on a healthy cluster.
        if !view.api_ready {
            return wait(format!(
                "waiting for the API server before upgrading {node_id}"
            ));
        }
        if let Some(unhealthy) = control_planes.iter().find(|cp| !cp.etcd_healthy) {
            return wait(format!(
                "waiting for etcd on {} before upgrading {node_id}",
                unhealthy.node_id
            ));
        }
        if member.state == STATE_DRAINED {
            return UpgradeStep::Apply { node_id };
        }
        // A tainted control plane still runs pods that tolerate the taint or
        // were scheduled before it was added.
        if !member.cordoned {
            return UpgradeStep::Drain { node_id };
        }
        return UpgradeStep::Apply { node_id };
    }
    UpgradeStep::Done
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VersionChange {
    None,
    /// The manifest asks for a new version.
    Start,
    /// Same version as stored, but some node has not finished moving to it.
    Resume,
}

/// What applying `next` does to a cluster whose stored version is `stored`.
/// `nodes` is each member's `(version, upgrade_state)`.
pub fn version_change(
    phase: &str,
    stored: &str,
    nodes: &[(String, String)],
    next: &str,
    membership_changes: bool,
    force: bool,
) -> Result<VersionChange, String> {
    kubernetes_versions::validate(next)?;
    let next_v = kubernetes_versions::effective(next);
    let stored_v = kubernetes_versions::effective(stored);
    let unfinished = nodes
        .iter()
        .any(|(version, state)| state != STATE_IDLE || !same_version(version, stored));

    if same_version(stored, next) {
        if !unfinished {
            return Ok(VersionChange::None);
        }
        if membership_changes {
            return Err(format!(
                "finish the upgrade to Kubernetes {stored_v} before adding or removing nodes"
            ));
        }
        return Ok(VersionChange::Resume);
    }

    if matches!(phase, "bootstrapping" | "scaling") {
        return Err(format!(
            "the cluster is {phase}; change the Kubernetes version after it is ready"
        ));
    }
    if phase == "upgrading" && !force {
        return Err(format!(
            "an upgrade to Kubernetes {stored_v} is in progress; wait for it, or pass --force to retarget it to {next_v}"
        ));
    }
    if membership_changes {
        return Err(
            "change the Kubernetes version and the node list in separate applies".to_string(),
        );
    }
    kubernetes_versions::check_upgrade(stored, next, force)?;
    for (version, _) in nodes {
        kubernetes_versions::check_upgrade(version, next, force)?;
    }
    Ok(VersionChange::Start)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn failure_summary_keeps_the_nix_error_line() {
        let message = "switch failed: building the system configuration...\n\
             … while evaluating the module\n\
             error: path '/nix/store/x-source/modules/kcore-keepalived-notrack.nix' does not exist\n";
        assert_eq!(
            apply_failure_summary(message),
            "error: path '/nix/store/x-source/modules/kcore-keepalived-notrack.nix' does not exist"
        );
        assert_eq!(
            apply_failure_summary("nixos-rebuild test+switch failed (exit 1)\n"),
            "nixos-rebuild test+switch failed (exit 1)"
        );
        assert_eq!(apply_failure_summary(""), "no error output");
        let long = "x".repeat(APPLY_SUMMARY_MAX + 10);
        assert_eq!(
            apply_failure_summary(&long).chars().count(),
            APPLY_SUMMARY_MAX + 1
        );
    }

    const FROM: &str = "1.35.2";
    const TO: &str = "1.36.6";

    fn member(id: &str, role: &str) -> UpgradeMember {
        UpgradeMember {
            node_id: id.to_string(),
            role: role.to_string(),
            version: FROM.to_string(),
            state: STATE_IDLE.to_string(),
            cordoned: false,
            apply: ApplyProgress::Unknown,
            apply_message: String::new(),
            kubelet_version: format!("v{FROM}"),
            node_ready: true,
            etcd_healthy: role != "worker",
        }
    }

    fn view(members: Vec<UpgradeMember>) -> UpgradeView {
        UpgradeView {
            desired: TO.to_string(),
            api_ready: true,
            members,
        }
    }

    /// Execute steps the way the orchestrator does, with every node behaving.
    fn drive(view: &mut UpgradeView) -> Vec<UpgradeStep> {
        let mut steps = Vec::new();
        for _ in 0..200 {
            let step = next_upgrade_step(view);
            steps.push(step.clone());
            let find = |view: &mut UpgradeView, id: &str| -> usize {
                view.members.iter().position(|m| m.node_id == id).unwrap()
            };
            match step {
                UpgradeStep::Done => return steps,
                UpgradeStep::Drain { node_id } => {
                    let i = find(view, &node_id);
                    view.members[i].cordoned = true;
                    view.members[i].state = STATE_DRAINED.to_string();
                }
                UpgradeStep::Apply { node_id } => {
                    let i = find(view, &node_id);
                    view.members[i].version = view.desired.clone();
                    view.members[i].state = STATE_APPLYING.to_string();
                    view.members[i].apply = ApplyProgress::Running;
                }
                UpgradeStep::Wait { node_id, .. } => {
                    let i = find(view, &node_id);
                    view.members[i].apply = ApplyProgress::Succeeded;
                    view.members[i].kubelet_version = format!("v{}", view.desired);
                }
                UpgradeStep::Uncordon { node_id } => {
                    let i = find(view, &node_id);
                    view.members[i].cordoned = false;
                }
                UpgradeStep::Finish { node_id } => {
                    let i = find(view, &node_id);
                    view.members[i].state = STATE_IDLE.to_string();
                }
                UpgradeStep::Fail { .. } => return steps,
            }
        }
        panic!("upgrade did not finish: {steps:?}");
    }

    fn applied_order(steps: &[UpgradeStep]) -> Vec<String> {
        steps
            .iter()
            .filter_map(|step| match step {
                UpgradeStep::Apply { node_id } => Some(node_id.clone()),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn control_planes_upgrade_before_workers_one_at_a_time() {
        let mut v = view(vec![
            member("w1", "worker"),
            member("cp2", "control-plane"),
            member("cp1", "control-plane"),
            member("w0", "worker"),
        ]);
        let steps = drive(&mut v);
        assert_eq!(applied_order(&steps), vec!["cp1", "cp2", "w0", "w1"]);
        assert!(v
            .members
            .iter()
            .all(|m| member_is_done(m, TO) && !m.cordoned));
    }

    #[test]
    fn every_node_is_drained_before_the_rebuild_and_uncordoned_after() {
        let mut v = view(vec![member("cp1", "control-plane"), member("w1", "worker")]);
        let steps = drive(&mut v);
        let pos = |wanted: UpgradeStep| steps.iter().position(|s| *s == wanted).unwrap();
        for id in ["cp1", "w1"] {
            let node_id = id.to_string();
            assert!(
                pos(UpgradeStep::Drain {
                    node_id: node_id.clone()
                }) < pos(UpgradeStep::Apply {
                    node_id: node_id.clone()
                })
            );
            assert!(
                pos(UpgradeStep::Apply {
                    node_id: node_id.clone()
                }) < pos(UpgradeStep::Uncordon { node_id })
            );
        }
    }

    #[test]
    fn the_next_node_waits_for_the_kubelet_to_report_the_new_version() {
        let mut cp = member("cp1", "control-plane");
        cp.version = TO.to_string();
        cp.state = STATE_APPLYING.to_string();
        cp.apply = ApplyProgress::Succeeded;
        let v = view(vec![cp, member("w1", "worker")]);
        match next_upgrade_step(&v) {
            UpgradeStep::Wait { node_id, message } => {
                assert_eq!(node_id, "cp1");
                assert!(message.contains("reports 1.35.2"), "{message}");
            }
            other => panic!("expected a wait, got {other:?}"),
        }
    }

    #[test]
    fn a_failed_rebuild_stops_the_rollout() {
        let mut cp = member("cp1", "control-plane");
        cp.version = TO.to_string();
        cp.state = STATE_APPLYING.to_string();
        cp.apply = ApplyProgress::Failed;
        cp.apply_message = "build error".to_string();
        let v = view(vec![cp, member("w1", "worker")]);
        assert!(matches!(
            next_upgrade_step(&v),
            UpgradeStep::Fail { ref node_id, ref message } if node_id == "cp1" && message.contains("build error")
        ));
    }

    #[test]
    fn a_failed_rebuild_reports_the_last_line_of_its_output() {
        let mut w = member("w1", "worker");
        w.version = TO.to_string();
        w.state = STATE_APPLYING.to_string();
        w.apply = ApplyProgress::Failed;
        let command = format!(
            "Command 'systemd-run --unit=nixos-rebuild-switch-to-configuration /nix/store/{}-nixos-system/bin/switch-to-configuration switch' returned non-zero exit status 4.",
            "x".repeat(300)
        );
        w.apply_message = format!(
            "switch failed: nixos-rebuild: warning: --fast is deprecated\nbuilding the system configuration...\n  /nix/store/abc-kubelet.drv\n{command}\n\n"
        );
        let mut cp = member("cp1", "control-plane");
        cp.version = TO.to_string();
        cp.kubelet_version = format!("v{TO}");
        let v = view(vec![cp, w]);
        let UpgradeStep::Fail { message, .. } = next_upgrade_step(&v) else {
            panic!("expected a failure");
        };
        assert!(
            message.ends_with("returned non-zero exit status 4."),
            "{message}"
        );
        assert!(!message.contains('\n'), "{message}");
        assert!(!message.contains("kubelet.drv"), "{message}");
        assert!(message.len() < 300, "{message}");
        assert_eq!(failure_summary(""), "nixos-rebuild failed");
    }

    #[test]
    fn a_failed_node_is_retried_from_the_start_on_resume() {
        let mut w = member("w1", "worker");
        w.version = TO.to_string();
        w.state = STATE_FAILED.to_string();
        let mut cp = member("cp1", "control-plane");
        cp.version = TO.to_string();
        cp.kubelet_version = format!("v{TO}");
        let v = view(vec![cp, w]);
        assert_eq!(
            next_upgrade_step(&v),
            UpgradeStep::Drain {
                node_id: "w1".to_string()
            }
        );
    }

    #[test]
    fn an_apply_the_controller_did_not_record_is_pushed_again() {
        let mut cp = member("cp1", "control-plane");
        cp.version = TO.to_string();
        cp.state = STATE_APPLYING.to_string();
        cp.apply = ApplyProgress::NotRecorded;
        assert_eq!(
            next_upgrade_step(&view(vec![cp])),
            UpgradeStep::Apply {
                node_id: "cp1".to_string()
            }
        );
    }

    #[test]
    fn nothing_starts_while_etcd_or_the_api_is_unhealthy() {
        let mut cp2 = member("cp2", "control-plane");
        cp2.etcd_healthy = false;
        let v = view(vec![member("cp1", "control-plane"), cp2.clone()]);
        assert!(
            matches!(next_upgrade_step(&v), UpgradeStep::Wait { ref message, .. } if message.contains("etcd on cp2"))
        );

        let mut v = view(vec![member("cp1", "control-plane")]);
        v.api_ready = false;
        assert!(matches!(next_upgrade_step(&v), UpgradeStep::Wait { .. }));
    }

    fn nodes(entries: &[(&str, &str)]) -> Vec<(String, String)> {
        entries
            .iter()
            .map(|(v, s)| (v.to_string(), s.to_string()))
            .collect()
    }

    #[test]
    fn a_new_version_starts_an_upgrade_and_the_same_one_is_a_no_op() {
        let idle = nodes(&[("", ""), ("", "")]);
        assert_eq!(
            version_change("ready", "", &idle, TO, false, false),
            Ok(VersionChange::Start)
        );
        assert_eq!(
            version_change("ready", "", &idle, "", false, false),
            Ok(VersionChange::None)
        );
        assert_eq!(
            version_change("ready", "", &idle, FROM, false, false),
            Ok(VersionChange::None)
        );
    }

    #[test]
    fn a_stopped_upgrade_resumes_on_the_same_version() {
        let mixed = nodes(&[(TO, ""), (TO, STATE_FAILED)]);
        assert_eq!(
            version_change("degraded", TO, &mixed, TO, false, false),
            Ok(VersionChange::Resume)
        );
        assert!(version_change("degraded", TO, &mixed, TO, true, false).is_err());
    }

    #[test]
    fn version_changes_are_refused_while_busy_or_with_membership_changes() {
        let idle = nodes(&[("", "")]);
        assert!(version_change("scaling", "", &idle, TO, false, false).is_err());
        assert!(version_change("bootstrapping", "", &idle, TO, false, true).is_err());
        assert!(version_change("ready", "", &idle, TO, true, false).is_err());
        let rolling = nodes(&[(TO, ""), ("", "")]);
        assert!(version_change("upgrading", TO, &rolling, "1.37.2", false, false).is_err());
    }

    #[test]
    fn every_node_is_checked_for_skips_and_downgrades() {
        let mixed = nodes(&[("1.36.6", ""), ("", "")]);
        assert!(version_change("degraded", "1.36.6", &mixed, "1.37.2", false, false).is_err());
        assert!(version_change(
            "ready",
            "1.36.6",
            &nodes(&[("1.36.6", "")]),
            "",
            false,
            false
        )
        .is_err());
        assert!(version_change(
            "ready",
            "1.36.6",
            &nodes(&[("1.36.6", "")]),
            "",
            false,
            true
        )
        .is_ok());
        assert!(version_change("ready", "", &nodes(&[("", "")]), "9.9.9", false, true).is_err());
    }

    #[test]
    fn a_cluster_already_on_the_version_is_done() {
        let mut cp = member("cp1", "control-plane");
        cp.version = String::new();
        let mut v = view(vec![cp]);
        v.desired = crate::kubernetes_versions::host_pin().to_string();
        assert_eq!(next_upgrade_step(&v), UpgradeStep::Done);
        assert_eq!(progress(&v), (1, 1));
    }
}
