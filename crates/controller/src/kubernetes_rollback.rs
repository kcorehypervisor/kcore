//! Rolling an upgrade back to the version the cluster ran before it.
//!
//! Only the nodes the upgrade already touched move, in the reverse of the
//! order they were upgraded: workers first, control planes last. The only
//! target is the stored previous version.

use std::cmp::Ordering;

use crate::kubernetes_versions;

/// `requested` must be the stored previous version, and an actual downgrade.
pub fn rollback_allowed(current: &str, previous: &str, requested: &str) -> Result<(), String> {
    if previous.trim().is_empty() {
        return Err("no previous version is recorded for this cluster".to_string());
    }
    if kubernetes_versions::compare(requested, previous) != Ordering::Equal {
        return Err(format!(
            "can only roll back to the previous version {}",
            kubernetes_versions::effective(previous)
        ));
    }
    if kubernetes_versions::compare(previous, current) != Ordering::Less {
        return Err(format!(
            "cluster is already on {}; nothing to roll back",
            kubernetes_versions::effective(current)
        ));
    }
    Ok(())
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RollbackMember {
    pub node_id: String,
    pub control_plane: bool,
    /// Version rendered into this node's Nix.
    pub version: String,
    pub etcd_healthy: bool,
    pub apply_running: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RollbackView {
    pub previous_version: String,
    /// Node ids in the order the upgrade moved them.
    pub upgrade_order: Vec<String>,
    pub members: Vec<RollbackMember>,
    pub api_ready: bool,
    /// Set by the caller when a rollback rebuild failed.
    pub failed: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RollbackStep {
    Wait { node_id: String, message: String },
    Rollback { node_id: String, to_version: String },
    Done,
    Failed { reason: String },
}

fn on_previous(member: &RollbackMember, previous: &str) -> bool {
    kubernetes_versions::compare(&member.version, previous) == Ordering::Equal
}

pub fn next_rollback_step(view: &RollbackView) -> RollbackStep {
    if let Some(reason) = &view.failed {
        return RollbackStep::Failed {
            reason: reason.clone(),
        };
    }
    let previous = kubernetes_versions::effective(&view.previous_version);

    for node_id in view.upgrade_order.iter().rev() {
        let Some(member) = view.members.iter().find(|m| &m.node_id == node_id) else {
            continue;
        };
        if on_previous(member, &previous) && !member.apply_running {
            continue;
        }
        let wait = |message: String| RollbackStep::Wait {
            node_id: node_id.clone(),
            message,
        };
        if member.apply_running {
            return wait(format!("rolling {node_id} back to Kubernetes {previous}"));
        }
        if member.control_plane {
            let control_planes: Vec<&RollbackMember> =
                view.members.iter().filter(|m| m.control_plane).collect();
            if let Some(other) = control_planes
                .iter()
                .find(|m| m.node_id != member.node_id && !m.etcd_healthy)
            {
                return wait(format!(
                    "waiting for etcd on {} before rolling back {node_id}",
                    other.node_id
                ));
            }
        } else if !view.api_ready {
            return wait(format!(
                "waiting for the API server before rolling back {node_id}"
            ));
        }
        return RollbackStep::Rollback {
            node_id: node_id.clone(),
            to_version: previous,
        };
    }
    RollbackStep::Done
}

#[cfg(test)]
mod tests {
    use super::*;

    const PREV: &str = "1.35.2";
    const NEW: &str = "1.36.6";

    fn member(id: &str, cp: bool, version: &str) -> RollbackMember {
        RollbackMember {
            node_id: id.to_string(),
            control_plane: cp,
            version: version.to_string(),
            etcd_healthy: cp,
            apply_running: false,
        }
    }

    fn view(members: Vec<RollbackMember>, order: &[&str]) -> RollbackView {
        RollbackView {
            previous_version: PREV.to_string(),
            upgrade_order: order.iter().map(|s| s.to_string()).collect(),
            members,
            api_ready: true,
            failed: None,
        }
    }

    fn drive(view: &mut RollbackView) -> Vec<RollbackStep> {
        let mut steps = Vec::new();
        for _ in 0..50 {
            let step = next_rollback_step(view);
            match &step {
                RollbackStep::Rollback {
                    node_id,
                    to_version,
                } => {
                    for m in view.members.iter_mut().filter(|m| &m.node_id == node_id) {
                        m.version = to_version.clone();
                    }
                }
                _ => {
                    steps.push(step);
                    return steps;
                }
            }
            steps.push(step);
        }
        panic!("rollback did not settle");
    }

    fn rollback(id: &str) -> RollbackStep {
        RollbackStep::Rollback {
            node_id: id.to_string(),
            to_version: PREV.to_string(),
        }
    }

    #[test]
    fn rolls_back_in_reverse_upgrade_order() {
        let mut v = view(
            vec![
                member("cp1", true, NEW),
                member("cp2", true, NEW),
                member("w1", false, NEW),
            ],
            &["cp1", "cp2", "w1"],
        );
        let steps = drive(&mut v);
        assert_eq!(
            steps,
            vec![
                rollback("w1"),
                rollback("cp2"),
                rollback("cp1"),
                RollbackStep::Done
            ]
        );
    }

    #[test]
    fn only_touched_nodes_move() {
        let mut v = view(
            vec![
                member("cp1", true, NEW),
                member("cp2", true, PREV),
                member("w1", false, PREV),
            ],
            &["cp1"],
        );
        let steps = drive(&mut v);
        assert_eq!(steps, vec![rollback("cp1"), RollbackStep::Done]);
    }

    #[test]
    fn nodes_outside_the_upgrade_are_left_alone() {
        let v = view(vec![member("cp1", true, NEW)], &[]);
        assert_eq!(next_rollback_step(&v), RollbackStep::Done);
    }

    #[test]
    fn already_on_previous_is_done() {
        let v = view(vec![member("cp1", true, PREV)], &["cp1"]);
        assert_eq!(next_rollback_step(&v), RollbackStep::Done);
    }

    #[test]
    fn waits_while_a_rebuild_runs() {
        let mut v = view(vec![member("w1", false, NEW)], &["w1"]);
        v.members[0].apply_running = true;
        assert!(matches!(next_rollback_step(&v), RollbackStep::Wait { .. }));
    }

    #[test]
    fn workers_wait_for_the_api() {
        let mut v = view(vec![member("w1", false, NEW)], &["w1"]);
        v.api_ready = false;
        assert!(matches!(next_rollback_step(&v), RollbackStep::Wait { .. }));
    }

    #[test]
    fn broken_control_plane_rolls_back_without_api() {
        let mut v = view(vec![member("cp1", true, NEW)], &["cp1"]);
        v.api_ready = false;
        assert_eq!(next_rollback_step(&v), rollback("cp1"));
    }

    #[test]
    fn control_plane_waits_for_other_etcd_members() {
        let mut v = view(
            vec![member("cp1", true, NEW), member("cp2", true, PREV)],
            &["cp1"],
        );
        v.members[1].etcd_healthy = false;
        assert!(matches!(next_rollback_step(&v), RollbackStep::Wait { .. }));
        v.members[1].etcd_healthy = true;
        assert_eq!(next_rollback_step(&v), rollback("cp1"));
    }

    #[test]
    fn failure_stops_the_rollback() {
        let mut v = view(vec![member("cp1", true, NEW)], &["cp1"]);
        v.failed = Some("rebuild failed".to_string());
        assert_eq!(
            next_rollback_step(&v),
            RollbackStep::Failed {
                reason: "rebuild failed".to_string()
            }
        );
    }

    #[test]
    fn partially_rolled_node_is_finished() {
        let mut v = view(vec![member("w1", false, "1.35.9")], &["w1"]);
        assert_eq!(next_rollback_step(&v), rollback("w1"));
        v.members[0].version = PREV.to_string();
        assert_eq!(next_rollback_step(&v), RollbackStep::Done);
    }

    #[test]
    fn allowed_only_to_the_previous_version() {
        assert!(rollback_allowed(NEW, PREV, PREV).is_ok());
        assert!(rollback_allowed(NEW, PREV, "1.34.0").is_err());
    }

    #[test]
    fn refused_without_a_previous_version() {
        assert!(rollback_allowed(NEW, "", PREV).is_err());
    }

    #[test]
    fn refused_when_previous_is_not_older() {
        assert!(rollback_allowed(PREV, PREV, PREV).is_err());
        assert!(rollback_allowed(PREV, NEW, NEW).is_err());
    }
}
