//! Decisions for adding or removing a Kubernetes member, and for cordoning a
//! member before a host generation is activated.
//!
//! The orchestrator and the cluster-update reconciler execute these steps.
//! They do not choose a different order.

use serde_json::Value;

pub const SINGLE_CONTROL_PLANE_UPDATE: &str =
    "This cluster has one control-plane node. Workloads are drained, the generation is applied, and the API is checked before the node is uncordoned.";

pub const LAST_CONTROL_PLANE: &str = "the last control-plane node cannot be removed";

pub const QUORUM_REFUSAL: &str = "removing this member would drop etcd below quorum";

/// A membership row plus what the live etcd and API lists say right now.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MemberSnapshot {
    pub node_id: String,
    pub role: String,
    pub pending_join: bool,
    pub pending_removal: bool,
    pub kubelet_ready: bool,
    pub kcore_exists: bool,
    pub kcore_ready: bool,
    pub in_etcd: bool,
    pub etcd_healthy: bool,
    pub in_api: bool,
    pub api_ready: bool,
    pub registered_name: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ScaleStep {
    Wait {
        node_id: String,
        message: String,
    },
    AddEtcd {
        node_id: String,
    },
    Configure {
        node_id: String,
    },
    WaitReady {
        node_id: String,
    },
    MarkJoined {
        node_id: String,
        registered_name: String,
    },
    CordonDrain {
        node_id: String,
    },
    RemoveEtcd {
        node_id: String,
    },
    DeleteApiNode {
        node_id: String,
    },
    DeleteMembership {
        node_id: String,
    },
    Refuse {
        node_id: String,
        message: String,
    },
    Ready,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DrainOutcome {
    NotRun,
    Ok,
    TimedOut,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ActivateOutcome {
    NotRun,
    Ok,
    Failed,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostUpdateView {
    pub is_member: bool,
    pub is_control_plane: bool,
    pub control_plane_count: usize,
    pub another_control_plane_draining: bool,
    pub phase: String,
    pub drain: DrainOutcome,
    pub activate: ActivateOutcome,
    pub node_ready: bool,
    pub on_generation: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HostAction {
    Activate,
    Drain { status: Option<&'static str> },
    UncordonAndFail,
    WaitReady,
    Uncordon,
    LeaveCordoned,
    WaitOtherControlPlane,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ListedNode {
    pub name: String,
    pub internal_ips: Vec<String>,
    pub ready: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PodOnNode {
    pub namespace: String,
    pub name: String,
    pub daemonset: bool,
    pub mirror: bool,
    pub empty_dir: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EtcdMember {
    pub id: String,
    pub name: String,
    pub peer_urls: Vec<String>,
}

pub fn phase_blocks_promotion(phase: &str) -> bool {
    matches!(phase, "ready" | "bootstrapping" | "updating" | "scaling")
}

pub fn refusal_status(message: &str) -> bool {
    message.contains(QUORUM_REFUSAL) || message.contains(LAST_CONTROL_PLANE)
}

/// `failed` from a refused scale stays failed. An older `failed` row whose
/// etcd and API server are already up can still be promoted.
pub fn promotion_blocked(phase: &str, message: &str) -> bool {
    phase_blocks_promotion(phase) || (phase == "failed" && refusal_status(message))
}

pub fn scaling_blocks_ready(message: &str) -> bool {
    let message = message.trim();
    message.starts_with("node ") || refusal_status(message)
}

pub fn scale_add_noop(member: &MemberSnapshot) -> bool {
    !member.pending_removal && member.kubelet_ready && member.in_api && member.api_ready
}

pub fn etcd_removal_refusal(members: &[MemberSnapshot], removing: &str) -> Option<&'static str> {
    let target = members.iter().find(|member| member.node_id == removing)?;
    if target.role == "worker" {
        return None;
    }
    let current: Vec<&str> = members
        .iter()
        .filter(|member| member.role != "worker" && member.in_etcd)
        .map(|member| member.node_id.as_str())
        .collect();
    if !current.contains(&removing) {
        return None;
    }
    let survivors: Vec<&str> = current.into_iter().filter(|id| *id != removing).collect();
    if survivors.is_empty() {
        return Some(LAST_CONTROL_PLANE);
    }
    let quorum = survivors.len() / 2 + 1;
    let healthy = members
        .iter()
        .filter(|member| survivors.contains(&member.node_id.as_str()) && member.etcd_healthy)
        .count();
    if healthy < quorum {
        Some(QUORUM_REFUSAL)
    } else {
        None
    }
}

pub fn next_scale_step(members: &[MemberSnapshot], drained: &[String]) -> ScaleStep {
    let mut order: Vec<&MemberSnapshot> = members.iter().collect();
    order.sort_by(|left, right| left.node_id.cmp(&right.node_id));

    if let Some(member) = order.iter().copied().find(|member| member.pending_removal) {
        if let Some(message) = etcd_removal_refusal(members, &member.node_id) {
            return ScaleStep::Refuse {
                node_id: member.node_id.clone(),
                message: message.to_string(),
            };
        }
        if member.in_api && !drained.iter().any(|id| id == &member.node_id) {
            return ScaleStep::CordonDrain {
                node_id: member.node_id.clone(),
            };
        }
        if member.role != "worker" && member.in_etcd {
            return ScaleStep::RemoveEtcd {
                node_id: member.node_id.clone(),
            };
        }
        if member.in_api {
            return ScaleStep::DeleteApiNode {
                node_id: member.node_id.clone(),
            };
        }
        return ScaleStep::DeleteMembership {
            node_id: member.node_id.clone(),
        };
    }

    if let Some(member) = order.iter().copied().find(|member| member.pending_join) {
        if !member.kcore_exists {
            return ScaleStep::Wait {
                node_id: member.node_id.clone(),
                message: format!("node {} is not an approved kcore node", member.node_id),
            };
        }
        if !member.kcore_ready {
            return ScaleStep::Wait {
                node_id: member.node_id.clone(),
                message: format!("node {} is not ready", member.node_id),
            };
        }
        let control_plane = member.role != "worker";
        if control_plane && !member.in_etcd {
            return ScaleStep::AddEtcd {
                node_id: member.node_id.clone(),
            };
        }
        if !member.in_api {
            return ScaleStep::Configure {
                node_id: member.node_id.clone(),
            };
        }
        if !member.api_ready || (control_plane && !member.etcd_healthy) {
            return ScaleStep::WaitReady {
                node_id: member.node_id.clone(),
            };
        }
        let registered_name = if member.registered_name.is_empty() {
            member.node_id.clone()
        } else {
            member.registered_name.clone()
        };
        return ScaleStep::MarkJoined {
            node_id: member.node_id.clone(),
            registered_name,
        };
    }

    ScaleStep::Ready
}

pub fn host_update_action(view: &HostUpdateView) -> HostAction {
    if view.another_control_plane_draining && view.is_member && view.is_control_plane {
        return HostAction::WaitOtherControlPlane;
    }
    match view.phase.as_str() {
        "prepared" => {
            if !view.is_member {
                return HostAction::Activate;
            }
            if view.drain == DrainOutcome::TimedOut {
                return HostAction::UncordonAndFail;
            }
            if view.drain == DrainOutcome::NotRun {
                let status = if view.is_control_plane && view.control_plane_count <= 1 {
                    Some(SINGLE_CONTROL_PLANE_UPDATE)
                } else {
                    None
                };
                return HostAction::Drain { status };
            }
            HostAction::Activate
        }
        "drained" => match view.activate {
            ActivateOutcome::Failed => HostAction::LeaveCordoned,
            ActivateOutcome::NotRun => HostAction::Activate,
            ActivateOutcome::Ok => HostAction::WaitReady,
        },
        "waiting_ready" => {
            if view.node_ready && view.on_generation {
                HostAction::Uncordon
            } else {
                HostAction::WaitReady
            }
        }
        _ => HostAction::WaitReady,
    }
}

pub fn generation_is_current(
    nixos_generation: &str,
    booted_generation: &str,
    target_generation: &str,
    reboot_required: bool,
) -> bool {
    if target_generation.is_empty() {
        return true;
    }
    if reboot_required {
        booted_generation == target_generation
    } else {
        nixos_generation == target_generation || booted_generation == target_generation
    }
}

pub fn pods_to_evict(pods: &[PodOnNode]) -> Vec<(String, String)> {
    pods.iter()
        .filter(|pod| !pod.daemonset && !pod.mirror)
        .map(|pod| (pod.namespace.clone(), pod.name.clone()))
        .collect()
}

pub fn pods_from_list(body: &str) -> Vec<PodOnNode> {
    let Ok(value) = serde_json::from_str::<Value>(body) else {
        return Vec::new();
    };
    let Some(items) = value.get("items").and_then(Value::as_array) else {
        return Vec::new();
    };
    items
        .iter()
        .filter_map(|item| {
            let meta = item.get("metadata")?;
            let name = meta.get("name")?.as_str()?.to_string();
            let namespace = meta
                .get("namespace")
                .and_then(Value::as_str)
                .unwrap_or("default")
                .to_string();
            let daemonset = meta
                .get("ownerReferences")
                .and_then(Value::as_array)
                .is_some_and(|owners| {
                    owners
                        .iter()
                        .any(|owner| owner.get("kind").and_then(Value::as_str) == Some("DaemonSet"))
                });
            let mirror = meta
                .get("annotations")
                .and_then(Value::as_object)
                .is_some_and(|notes| notes.contains_key("kubernetes.io/config.mirror"));
            let empty_dir = item
                .get("spec")
                .and_then(|spec| spec.get("volumes"))
                .and_then(Value::as_array)
                .is_some_and(|volumes| {
                    volumes
                        .iter()
                        .any(|volume| volume.get("emptyDir").is_some())
                });
            Some(PodOnNode {
                namespace,
                name,
                daemonset,
                mirror,
                empty_dir,
            })
        })
        .collect()
}

pub fn nodes_from_list(body: &str) -> Vec<ListedNode> {
    let Ok(value) = serde_json::from_str::<Value>(body) else {
        return Vec::new();
    };
    let items = if let Some(items) = value.get("items").and_then(Value::as_array) {
        items.clone()
    } else if value.get("metadata").is_some() {
        vec![value]
    } else {
        return Vec::new();
    };
    items
        .iter()
        .filter_map(|item| {
            let name = item
                .pointer("/metadata/name")
                .and_then(Value::as_str)?
                .to_string();
            let internal_ips = item
                .pointer("/status/addresses")
                .and_then(Value::as_array)
                .map(|addresses| {
                    addresses
                        .iter()
                        .filter(|address| {
                            address.get("type").and_then(Value::as_str) == Some("InternalIP")
                        })
                        .filter_map(|address| {
                            address
                                .get("address")
                                .and_then(Value::as_str)
                                .map(str::to_string)
                        })
                        .collect()
                })
                .unwrap_or_default();
            let ready = item
                .pointer("/status/conditions")
                .and_then(Value::as_array)
                .is_some_and(|conditions| {
                    conditions.iter().any(|condition| {
                        condition.get("type").and_then(Value::as_str) == Some("Ready")
                            && condition.get("status").and_then(Value::as_str) == Some("True")
                    })
                });
            Some(ListedNode {
                name,
                internal_ips,
                ready,
            })
        })
        .collect()
}

pub fn match_registered_node<'a>(
    nodes: &'a [ListedNode],
    stored_name: &str,
    hostname: &str,
    ip: &str,
) -> Option<&'a ListedNode> {
    if !stored_name.is_empty() {
        if let Some(found) = nodes.iter().find(|node| node.name == stored_name) {
            return Some(found);
        }
    }
    if !ip.is_empty() {
        if let Some(found) = nodes
            .iter()
            .find(|node| node.internal_ips.iter().any(|addr| addr == ip))
        {
            return Some(found);
        }
    }
    if !hostname.is_empty() {
        return nodes.iter().find(|node| node.name == hostname);
    }
    None
}

pub fn etcd_members_from_list(body: &str) -> Vec<EtcdMember> {
    let Ok(value) = serde_json::from_str::<Value>(body) else {
        return Vec::new();
    };
    let Some(members) = value.get("members").and_then(Value::as_array) else {
        return Vec::new();
    };
    members
        .iter()
        .filter_map(|member| {
            let id = match member.get("ID") {
                Some(Value::String(id)) => id.clone(),
                Some(Value::Number(id)) => id.to_string(),
                _ => return None,
            };
            let name = member
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            let peer_urls = member
                .get("peerURLs")
                .and_then(Value::as_array)
                .map(|urls| {
                    urls.iter()
                        .filter_map(Value::as_str)
                        .map(str::to_string)
                        .collect()
                })
                .unwrap_or_default();
            Some(EtcdMember {
                id,
                name,
                peer_urls,
            })
        })
        .collect()
}

pub fn member_for_ip<'a>(members: &'a [EtcdMember], ip: &str) -> Option<&'a EtcdMember> {
    if ip.is_empty() {
        return None;
    }
    let marker = format!("//{ip}:");
    members.iter().find(|member| {
        member
            .peer_urls
            .iter()
            .any(|url| url.contains(&marker) || url.ends_with(&format!("//{ip}")))
    })
}

pub struct MembershipDelta {
    pub add: Vec<(String, String)>,
    pub remove: Vec<String>,
}

pub fn membership_delta(
    existing: &[(String, String)],
    desired: &[(String, String)],
) -> Result<MembershipDelta, String> {
    for (id, role) in desired {
        if let Some((_, current)) = existing.iter().find(|(node_id, _)| node_id == id) {
            if current != role {
                return Err(format!(
                    "node {id} cannot change role from {current} to {role}"
                ));
            }
        }
    }
    let add = desired
        .iter()
        .filter(|(id, _)| !existing.iter().any(|(node_id, _)| node_id == id))
        .cloned()
        .collect();
    let remove = existing
        .iter()
        .filter(|(id, _)| !desired.iter().any(|(node_id, _)| node_id == id))
        .map(|(id, _)| id.clone())
        .collect();
    Ok(MembershipDelta { add, remove })
}

fn member(id: &str, role: &str) -> MemberSnapshot {
    MemberSnapshot {
        node_id: id.to_string(),
        role: role.to_string(),
        pending_join: false,
        pending_removal: false,
        kubelet_ready: true,
        kcore_exists: true,
        kcore_ready: true,
        in_etcd: role != "worker",
        etcd_healthy: role != "worker",
        in_api: true,
        api_ready: true,
        registered_name: id.to_string(),
    }
}

fn drive(members: &mut Vec<MemberSnapshot>) -> Vec<ScaleStep> {
    let mut drained = Vec::new();
    let mut steps = Vec::new();
    for _ in 0..24 {
        let step = next_scale_step(members, &drained);
        steps.push(step.clone());
        match step {
            ScaleStep::Ready
            | ScaleStep::Wait { .. }
            | ScaleStep::WaitReady { .. }
            | ScaleStep::Refuse { .. } => break,
            ScaleStep::AddEtcd { node_id } => {
                let row = members
                    .iter_mut()
                    .find(|row| row.node_id == node_id)
                    .unwrap();
                row.in_etcd = true;
            }
            ScaleStep::Configure { node_id } => {
                let row = members
                    .iter_mut()
                    .find(|row| row.node_id == node_id)
                    .unwrap();
                row.in_api = true;
                row.api_ready = true;
                if row.role != "worker" {
                    row.etcd_healthy = true;
                }
                row.registered_name = format!("registered-{node_id}");
            }
            ScaleStep::MarkJoined {
                node_id,
                registered_name,
            } => {
                let row = members
                    .iter_mut()
                    .find(|row| row.node_id == node_id)
                    .unwrap();
                row.pending_join = false;
                row.kubelet_ready = true;
                row.registered_name = registered_name;
            }
            ScaleStep::CordonDrain { node_id } => drained.push(node_id),
            ScaleStep::RemoveEtcd { node_id } => {
                let row = members
                    .iter_mut()
                    .find(|row| row.node_id == node_id)
                    .unwrap();
                row.in_etcd = false;
                row.etcd_healthy = false;
            }
            ScaleStep::DeleteApiNode { node_id } => {
                let row = members
                    .iter_mut()
                    .find(|row| row.node_id == node_id)
                    .unwrap();
                row.in_api = false;
                row.api_ready = false;
            }
            ScaleStep::DeleteMembership { node_id } => {
                members.retain(|row| row.node_id != node_id);
            }
        }
    }
    steps
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scaling_is_not_promoted_to_ready() {
        assert!(phase_blocks_promotion("scaling"));
        assert!(phase_blocks_promotion("bootstrapping"));
        assert!(promotion_blocked(
            "failed",
            "removing this member would drop etcd below quorum"
        ));
        assert!(!promotion_blocked("failed", "etcd was down"));
        assert!(scaling_blocks_ready(
            "node worker-2 is not an approved kcore node"
        ));
    }

    #[test]
    fn ready_cluster_scaled_by_one_worker_ends_ready_without_a_second_kubelet() {
        let mut worker = member("worker-2", "worker");
        worker.pending_join = true;
        worker.kubelet_ready = false;
        worker.in_etcd = false;
        worker.etcd_healthy = false;
        worker.in_api = false;
        worker.api_ready = false;
        worker.registered_name.clear();
        let mut members = vec![member("cp-1", "control-plane"), worker];
        let steps = drive(&mut members);
        let configures = steps
            .iter()
            .filter(|step| matches!(step, ScaleStep::Configure { .. }))
            .count();
        assert_eq!(configures, 1);
        assert!(steps
            .iter()
            .all(|step| !matches!(step, ScaleStep::AddEtcd { .. })));
        assert!(matches!(steps.last(), Some(ScaleStep::Ready)));
        let joined = members
            .iter()
            .find(|row| row.node_id == "worker-2")
            .unwrap();
        assert!(joined.in_api);
        assert!(joined.api_ready);
        assert!(joined.kubelet_ready);
        assert!(!joined.pending_join);
        assert_eq!(joined.registered_name, "registered-worker-2");
        assert!(scale_add_noop(joined));
        let again = drive(&mut members);
        assert_eq!(again, vec![ScaleStep::Ready]);
        assert_eq!(configures, 1);
    }

    #[test]
    fn adding_a_control_plane_grows_etcd_by_one_and_quorum_refusal_keeps_the_member() {
        let mut joining = member("cp-2", "control-plane");
        joining.pending_join = true;
        joining.kubelet_ready = false;
        joining.in_etcd = false;
        joining.etcd_healthy = false;
        joining.in_api = false;
        joining.api_ready = false;
        let before = 1;
        let mut members = vec![member("cp-1", "control-plane"), joining];
        let steps = drive(&mut members);
        assert!(steps
            .iter()
            .any(|step| matches!(step, ScaleStep::AddEtcd { node_id } if node_id == "cp-2")));
        let etcd_members = members.iter().filter(|row| row.in_etcd).count();
        assert_eq!(etcd_members, before + 1);
        assert!(matches!(steps.last(), Some(ScaleStep::Ready)));

        let mut only = member("cp-1", "control-plane");
        only.pending_removal = true;
        let mut cluster = vec![only];
        let refused = drive(&mut cluster);
        assert!(matches!(
            refused.first(),
            Some(ScaleStep::Refuse { message, .. }) if message == LAST_CONTROL_PLANE
        ));
        assert!(cluster
            .iter()
            .any(|row| row.node_id == "cp-1" && row.in_etcd));

        let mut first = member("cp-1", "control-plane");
        first.pending_removal = true;
        let mut second = member("cp-2", "control-plane");
        second.etcd_healthy = false;
        let mut third = member("cp-3", "control-plane");
        third.etcd_healthy = false;
        let mut trio = vec![first, second, third];
        let quorum = drive(&mut trio);
        assert!(matches!(
            quorum.first(),
            Some(ScaleStep::Refuse { message, .. }) if message == QUORUM_REFUSAL
        ));
        assert_eq!(trio.iter().filter(|row| row.in_etcd).count(), 3);
        assert!(trio.iter().any(|row| row.node_id == "cp-1"));
    }

    #[test]
    fn removing_a_worker_cordons_drains_and_deletes_the_node_before_membership() {
        let mut worker = member("worker-2", "worker");
        worker.pending_removal = true;
        worker.in_etcd = false;
        worker.etcd_healthy = false;
        let mut members = vec![member("cp-1", "control-plane"), worker];
        let mut drained = Vec::new();
        let mut saw_api_delete = false;
        for _ in 0..8 {
            let step = next_scale_step(&members, &drained);
            match step {
                ScaleStep::CordonDrain { node_id } => {
                    assert!(!saw_api_delete);
                    drained.push(node_id);
                }
                ScaleStep::DeleteApiNode { node_id } => {
                    assert!(drained.iter().any(|id| id == &node_id));
                    let row = members
                        .iter_mut()
                        .find(|row| row.node_id == node_id)
                        .unwrap();
                    assert!(row.in_api);
                    row.in_api = false;
                    row.api_ready = false;
                    saw_api_delete = true;
                }
                ScaleStep::DeleteMembership { node_id } => {
                    assert!(saw_api_delete);
                    assert!(members
                        .iter()
                        .any(|row| row.node_id == node_id && !row.in_api));
                    members.retain(|row| row.node_id != node_id);
                    break;
                }
                ScaleStep::RemoveEtcd { .. } => panic!("a worker removal must not touch etcd"),
                other => panic!("unexpected step {other:?}"),
            }
        }
        assert!(saw_api_delete);
        assert!(!members.iter().any(|row| row.node_id == "worker-2"));
    }

    #[test]
    fn cluster_update_of_a_member_drains_before_activate_and_timeout_does_not_activate() {
        let prepared = HostUpdateView {
            is_member: true,
            is_control_plane: false,
            control_plane_count: 3,
            another_control_plane_draining: false,
            phase: "prepared".into(),
            drain: DrainOutcome::NotRun,
            activate: ActivateOutcome::NotRun,
            node_ready: true,
            on_generation: false,
        };
        assert!(matches!(
            host_update_action(&prepared),
            HostAction::Drain { status: None }
        ));
        let drained = HostUpdateView {
            phase: "drained".into(),
            drain: DrainOutcome::Ok,
            ..prepared.clone()
        };
        assert_eq!(host_update_action(&drained), HostAction::Activate);
        let waiting = HostUpdateView {
            phase: "waiting_ready".into(),
            activate: ActivateOutcome::Ok,
            node_ready: false,
            on_generation: false,
            ..drained
        };
        assert_eq!(host_update_action(&waiting), HostAction::WaitReady);
        let ready = HostUpdateView {
            node_ready: true,
            on_generation: true,
            ..waiting
        };
        assert_eq!(host_update_action(&ready), HostAction::Uncordon);

        let timed_out = HostUpdateView {
            drain: DrainOutcome::TimedOut,
            ..prepared
        };
        assert_eq!(host_update_action(&timed_out), HostAction::UncordonAndFail);
        let failed_activate = HostUpdateView {
            phase: "drained".into(),
            drain: DrainOutcome::Ok,
            activate: ActivateOutcome::Failed,
            ..timed_out
        };
        assert_eq!(
            host_update_action(&failed_activate),
            HostAction::LeaveCordoned
        );
    }

    #[test]
    fn a_node_that_is_not_a_member_is_updated_without_a_cordon() {
        let action = host_update_action(&HostUpdateView {
            is_member: false,
            is_control_plane: false,
            control_plane_count: 0,
            another_control_plane_draining: false,
            phase: "prepared".into(),
            drain: DrainOutcome::NotRun,
            activate: ActivateOutcome::NotRun,
            node_ready: false,
            on_generation: false,
        });
        assert_eq!(action, HostAction::Activate);
    }

    #[test]
    fn one_control_plane_records_the_status_and_a_second_is_not_cordoned() {
        let only = host_update_action(&HostUpdateView {
            is_member: true,
            is_control_plane: true,
            control_plane_count: 1,
            another_control_plane_draining: false,
            phase: "prepared".into(),
            drain: DrainOutcome::NotRun,
            activate: ActivateOutcome::NotRun,
            node_ready: true,
            on_generation: false,
        });
        assert_eq!(
            only,
            HostAction::Drain {
                status: Some(SINGLE_CONTROL_PLANE_UPDATE)
            }
        );
        let blocked = host_update_action(&HostUpdateView {
            control_plane_count: 3,
            another_control_plane_draining: true,
            ..HostUpdateView {
                is_member: true,
                is_control_plane: true,
                control_plane_count: 3,
                another_control_plane_draining: true,
                phase: "prepared".into(),
                drain: DrainOutcome::NotRun,
                activate: ActivateOutcome::NotRun,
                node_ready: true,
                on_generation: false,
            }
        });
        assert_eq!(blocked, HostAction::WaitOtherControlPlane);
    }

    #[test]
    fn drain_evicts_emptydir_pods_and_skips_daemonsets() {
        let pods = pods_from_list(
            r#"{"items":[
                {"metadata":{"name":"web","namespace":"default"},"spec":{"volumes":[{"name":"scratch","emptyDir":{}}]}},
                {"metadata":{"name":"cilium","namespace":"kube-system","ownerReferences":[{"kind":"DaemonSet","name":"cilium"}]}},
                {"metadata":{"name":"mirror","namespace":"default","annotations":{"kubernetes.io/config.mirror":"1"}}},
                {"metadata":{"name":"job","namespace":"batch"}}
            ]}"#,
        );
        let evict = pods_to_evict(&pods);
        assert!(pods.iter().any(|pod| pod.name == "web" && pod.empty_dir));
        assert_eq!(
            evict,
            vec![
                ("default".to_string(), "web".to_string()),
                ("batch".to_string(), "job".to_string())
            ]
        );
    }

    #[test]
    fn etcd_member_list_matches_the_peer_ip_and_registered_names_differ_from_node_id() {
        let members = etcd_members_from_list(
            r#"{"members":[{"ID":"10276657743932975437","name":"cp-1","peerURLs":["https://192.168.40.105:2380"]},{"ID":"99","name":"cp-2","peerURLs":["https://192.168.40.107:2380"]}]}"#,
        );
        let found = member_for_ip(&members, "192.168.40.107").unwrap();
        assert_eq!(found.id, "99");
        assert!(member_for_ip(&members, "192.168.40.10").is_none());
        let nodes = nodes_from_list(
            r#"{"items":[{"metadata":{"name":"registered-worker"},"status":{"addresses":[{"type":"InternalIP","address":"192.168.40.110"}],"conditions":[{"type":"Ready","status":"True"}]}}]}"#,
        );
        let matched = match_registered_node(&nodes, "", "worker-2", "192.168.40.110").unwrap();
        assert_eq!(matched.name, "registered-worker");
        assert!(matched.ready);
        assert_ne!(matched.name, "worker-2");
    }

    #[test]
    fn generation_waits_for_the_booted_system_when_reboot_is_required() {
        assert!(
            generation_is_current(
                "/nix/var/nix/profiles/system-12-link",
                "",
                "system-12-link",
                false
            ) || generation_is_current("system-12-link", "", "system-12-link", false)
        );
        assert!(generation_is_current(
            "system-12-link",
            "system-11-link",
            "system-12-link",
            false
        ));
        assert!(!generation_is_current(
            "system-12-link",
            "system-11-link",
            "system-12-link",
            true
        ));
        assert!(generation_is_current(
            "system-12-link",
            "system-12-link",
            "system-12-link",
            true
        ));
    }
}
