//! Rolling Kubernetes certificate renewal, one node at a time.
//!
//! Control planes go first, in node id order, then workers. A node is renewed
//! (new leaf certificates rendered and applied), then its units restart. A
//! control plane is only touched while etcd keeps a quorum without it.

use time::{Duration, OffsetDateTime};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KubernetesCertConfig {
    pub leaf_validity_days: i64,
    pub renew_before_days: i64,
}

impl Default for KubernetesCertConfig {
    fn default() -> Self {
        Self {
            leaf_validity_days: 365,
            renew_before_days: 30,
        }
    }
}

/// Due once expired or when `renew_before_days` or fewer remain.
pub fn renewal_due(now: OffsetDateTime, not_after: OffsetDateTime, renew_before_days: i64) -> bool {
    now >= not_after || not_after - now <= Duration::days(renew_before_days)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RenewalMember {
    pub node_id: String,
    pub control_plane: bool,
    pub cert_due: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RenewalView {
    pub members: Vec<RenewalMember>,
    /// Nodes whose certificates were already replaced in this roll.
    pub renewed: Vec<String>,
    /// Nodes whose units already restarted onto the new certificates.
    pub restarted: Vec<String>,
    pub api_ready: bool,
    pub etcd_healthy_cps: usize,
    /// The node with a rebuild in flight, if any.
    pub applying: Option<String>,
    /// Set by the caller when the rebuild of a node failed.
    pub failed: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RenewalStep {
    /// Nothing to do now: no certificate is due, or the roll must wait.
    Idle,
    RenewNode {
        node_id: String,
    },
    RestartUnits {
        node_id: String,
    },
    Done,
    Failed {
        reason: String,
    },
}

fn rollout_order(members: &[RenewalMember]) -> Vec<&RenewalMember> {
    let mut ordered: Vec<&RenewalMember> = members.iter().collect();
    ordered.sort_by(|a, b| {
        b.control_plane
            .cmp(&a.control_plane)
            .then_with(|| a.node_id.cmp(&b.node_id))
    });
    ordered
}

fn has(list: &[String], node_id: &str) -> bool {
    list.iter().any(|n| n == node_id)
}

/// A control plane may go down only if etcd keeps a majority without it.
fn etcd_allows(view: &RenewalView, member: &RenewalMember) -> bool {
    if !member.control_plane {
        return true;
    }
    let total = view.members.iter().filter(|m| m.control_plane).count();
    let quorum = total / 2 + 1;
    if total <= 1 {
        return view.etcd_healthy_cps >= 1;
    }
    view.etcd_healthy_cps > quorum
}

pub fn next_renewal_step(view: &RenewalView) -> RenewalStep {
    if let Some(reason) = &view.failed {
        return RenewalStep::Failed {
            reason: reason.clone(),
        };
    }
    if view.applying.is_some() {
        return RenewalStep::Idle;
    }

    let order = rollout_order(&view.members);
    let in_flight = order
        .iter()
        .find(|m| has(&view.renewed, &m.node_id) && !has(&view.restarted, &m.node_id));
    let pending = order
        .iter()
        .find(|m| m.cert_due && !has(&view.renewed, &m.node_id));

    let Some(member) = in_flight.or(pending) else {
        let rolled = order.iter().any(|m| has(&view.renewed, &m.node_id));
        return if rolled {
            RenewalStep::Done
        } else {
            RenewalStep::Idle
        };
    };

    if !view.api_ready || !etcd_allows(view, member) {
        return RenewalStep::Idle;
    }
    let node_id = member.node_id.clone();
    if has(&view.renewed, &node_id) {
        RenewalStep::RestartUnits { node_id }
    } else {
        RenewalStep::RenewNode { node_id }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn member(id: &str, cp: bool, due: bool) -> RenewalMember {
        RenewalMember {
            node_id: id.to_string(),
            control_plane: cp,
            cert_due: due,
        }
    }

    fn view(members: Vec<RenewalMember>) -> RenewalView {
        let cps = members.iter().filter(|m| m.control_plane).count();
        RenewalView {
            members,
            renewed: vec![],
            restarted: vec![],
            api_ready: true,
            etcd_healthy_cps: cps,
            applying: None,
            failed: None,
        }
    }

    /// Applies each step the way the orchestrator would and records them.
    fn drive(view: &mut RenewalView, limit: usize) -> (Vec<RenewalStep>, RenewalStep) {
        let mut steps = Vec::new();
        for _ in 0..limit {
            let step = next_renewal_step(view);
            match &step {
                RenewalStep::RenewNode { node_id } => {
                    view.renewed.push(node_id.clone());
                    for m in view.members.iter_mut().filter(|m| &m.node_id == node_id) {
                        m.cert_due = false;
                    }
                }
                RenewalStep::RestartUnits { node_id } => view.restarted.push(node_id.clone()),
                _ => return (steps, step),
            }
            steps.push(step);
        }
        panic!("renewal did not settle");
    }

    fn renew(id: &str) -> RenewalStep {
        RenewalStep::RenewNode {
            node_id: id.to_string(),
        }
    }

    fn restart(id: &str) -> RenewalStep {
        RenewalStep::RestartUnits {
            node_id: id.to_string(),
        }
    }

    #[test]
    fn default_config() {
        let cfg = KubernetesCertConfig::default();
        assert_eq!(cfg.leaf_validity_days, 365);
        assert_eq!(cfg.renew_before_days, 30);
    }

    #[test]
    fn due_inside_window() {
        let now = OffsetDateTime::UNIX_EPOCH + Duration::days(1000);
        assert!(renewal_due(now, now + Duration::days(30), 30));
        assert!(renewal_due(now, now + Duration::days(5), 30));
        assert!(!renewal_due(now, now + Duration::days(31), 30));
    }

    #[test]
    fn due_when_expired() {
        let now = OffsetDateTime::UNIX_EPOCH + Duration::days(1000);
        assert!(renewal_due(now, now, 0));
        assert!(renewal_due(now, now - Duration::days(1), 0));
    }

    #[test]
    fn nothing_due_is_idle() {
        let mut v = view(vec![member("cp1", true, false), member("w1", false, false)]);
        let (steps, last) = drive(&mut v, 10);
        assert!(steps.is_empty());
        assert_eq!(last, RenewalStep::Idle);
    }

    #[test]
    fn control_planes_roll_before_workers_one_at_a_time() {
        let mut v = view(vec![
            member("w1", false, true),
            member("cp2", true, true),
            member("cp1", true, true),
            member("cp3", true, true),
        ]);
        let (steps, last) = drive(&mut v, 20);
        assert_eq!(
            steps,
            vec![
                renew("cp1"),
                restart("cp1"),
                renew("cp2"),
                restart("cp2"),
                renew("cp3"),
                restart("cp3"),
                renew("w1"),
                restart("w1"),
            ]
        );
        assert_eq!(last, RenewalStep::Done);
    }

    #[test]
    fn only_due_nodes_roll() {
        let mut v = view(vec![
            member("cp1", true, false),
            member("cp2", true, true),
            member("cp3", true, false),
        ]);
        let (steps, last) = drive(&mut v, 10);
        assert_eq!(steps, vec![renew("cp2"), restart("cp2")]);
        assert_eq!(last, RenewalStep::Done);
    }

    #[test]
    fn waits_while_a_node_applies() {
        let mut v = view(vec![member("cp1", true, true)]);
        v.applying = Some("cp1".to_string());
        assert_eq!(next_renewal_step(&v), RenewalStep::Idle);
    }

    #[test]
    fn waits_for_api() {
        let mut v = view(vec![member("cp1", true, true)]);
        v.api_ready = false;
        assert_eq!(next_renewal_step(&v), RenewalStep::Idle);
    }

    #[test]
    fn waits_when_etcd_would_lose_quorum() {
        let mut v = view(vec![
            member("cp1", true, true),
            member("cp2", true, false),
            member("cp3", true, false),
        ]);
        v.etcd_healthy_cps = 2;
        assert_eq!(next_renewal_step(&v), RenewalStep::Idle);
        v.etcd_healthy_cps = 3;
        assert_eq!(next_renewal_step(&v), renew("cp1"));
    }

    #[test]
    fn single_control_plane_rolls_when_etcd_is_up() {
        let mut v = view(vec![member("cp1", true, true)]);
        v.etcd_healthy_cps = 0;
        assert_eq!(next_renewal_step(&v), RenewalStep::Idle);
        v.etcd_healthy_cps = 1;
        let (steps, last) = drive(&mut v, 5);
        assert_eq!(steps, vec![renew("cp1"), restart("cp1")]);
        assert_eq!(last, RenewalStep::Done);
    }

    #[test]
    fn workers_do_not_need_etcd() {
        let mut v = view(vec![member("w1", false, true)]);
        v.etcd_healthy_cps = 0;
        assert_eq!(next_renewal_step(&v), renew("w1"));
    }

    #[test]
    fn restart_comes_before_the_next_node() {
        let mut v = view(vec![
            member("cp1", true, false),
            member("cp2", true, true),
            member("cp3", true, false),
        ]);
        v.renewed = vec!["cp1".to_string()];
        assert_eq!(next_renewal_step(&v), restart("cp1"));
    }

    #[test]
    fn resumes_after_controller_restart() {
        let mut v = view(vec![
            member("cp1", true, false),
            member("cp2", true, true),
            member("cp3", true, true),
        ]);
        v.renewed = vec!["cp1".to_string()];
        v.restarted = vec!["cp1".to_string()];
        let (steps, last) = drive(&mut v, 10);
        assert_eq!(
            steps,
            vec![renew("cp2"), restart("cp2"), renew("cp3"), restart("cp3")]
        );
        assert_eq!(last, RenewalStep::Done);
    }

    #[test]
    fn failure_stops_the_roll() {
        let mut v = view(vec![member("cp1", true, true)]);
        v.failed = Some("rebuild failed on cp1".to_string());
        assert_eq!(
            next_renewal_step(&v),
            RenewalStep::Failed {
                reason: "rebuild failed on cp1".to_string()
            }
        );
    }

    #[test]
    fn finished_roll_stays_done() {
        let mut v = view(vec![member("cp1", true, true)]);
        drive(&mut v, 5);
        assert_eq!(next_renewal_step(&v), RenewalStep::Done);
    }
}
