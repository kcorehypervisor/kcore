//! When to replace a dead control plane.
//!
//! Replacing a member is only safe while etcd still has a quorum without it
//! and exactly one control plane is down. Anything else is an alert for the
//! operator, not an automatic action.

use serde::{Deserialize, Serialize};

/// Stored `repair_json`. Empty means repair is off.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RepairPolicy {
    /// Seconds a control plane must stay unhealthy before it is replaced.
    #[serde(default = "default_after_secs")]
    pub after_secs: i64,
    /// Registered kcore node ids that may take over a dead control plane.
    #[serde(default)]
    pub spare_pool: Vec<String>,
}

fn default_after_secs() -> i64 {
    600
}

pub fn parse_repair_policy(json: &str) -> Result<Option<RepairPolicy>, String> {
    if json.trim().is_empty() {
        return Ok(None);
    }
    let policy: RepairPolicy =
        serde_json::from_str(json).map_err(|e| format!("invalid repair policy: {e}"))?;
    if policy.after_secs < 60 {
        return Err("repair after_secs must be at least 60".to_string());
    }
    Ok(Some(policy))
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CpHealth {
    pub node_id: String,
    pub healthy: bool,
    /// Unix seconds since the control plane stopped being healthy.
    pub unhealthy_since: Option<i64>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RepairDecision {
    None,
    Alert { node_id: String, reason: String },
    Replace { old: String, new_spare: String },
}

/// A control plane is dead once it has been unhealthy for `after_secs`.
pub fn repair_decision(
    cps: &[CpHealth],
    now: i64,
    after_secs: i64,
    spare_pool: &[String],
    busy: bool,
) -> RepairDecision {
    if busy {
        return RepairDecision::None;
    }

    let unhealthy: Vec<&CpHealth> = cps.iter().filter(|cp| !cp.healthy).collect();
    let dead: Vec<&CpHealth> = unhealthy
        .iter()
        .copied()
        .filter(|cp| {
            cp.unhealthy_since
                .is_some_and(|since| now - since >= after_secs)
        })
        .collect();
    let Some(oldest) = dead.iter().min_by_key(|cp| cp.unhealthy_since) else {
        return RepairDecision::None;
    };
    let node_id = oldest.node_id.clone();

    let quorum = cps.len() / 2 + 1;
    let healthy = cps.len() - unhealthy.len();
    if healthy < quorum {
        return RepairDecision::Alert {
            node_id,
            reason: format!(
                "etcd has {healthy} of {} members healthy, quorum is {quorum}; restore from a backup instead of replacing",
                cps.len()
            ),
        };
    }
    if unhealthy.len() > 1 {
        return RepairDecision::Alert {
            node_id,
            reason: format!(
                "{} control planes are unhealthy; replace them one at a time by hand",
                unhealthy.len()
            ),
        };
    }

    match spare_pool
        .iter()
        .find(|spare| !cps.iter().any(|cp| &cp.node_id == *spare))
    {
        Some(spare) => RepairDecision::Replace {
            old: node_id,
            new_spare: spare.clone(),
        },
        None => RepairDecision::Alert {
            node_id,
            reason: "control plane is down and the spare pool is empty".to_string(),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn policy_parses_with_defaults_and_rejects_short_delays() {
        assert_eq!(parse_repair_policy("").unwrap(), None);
        let p = parse_repair_policy(r#"{"spare_pool":["s1"]}"#)
            .unwrap()
            .unwrap();
        assert_eq!(p.after_secs, 600);
        assert_eq!(p.spare_pool, vec!["s1".to_string()]);
        assert!(parse_repair_policy(r#"{"after_secs":5}"#).is_err());
    }

    const AFTER: i64 = 600;

    fn up(id: &str) -> CpHealth {
        CpHealth {
            node_id: id.to_string(),
            healthy: true,
            unhealthy_since: None,
        }
    }

    fn down(id: &str, since: i64) -> CpHealth {
        CpHealth {
            node_id: id.to_string(),
            healthy: false,
            unhealthy_since: Some(since),
        }
    }

    fn spares(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn all_healthy_does_nothing() {
        let cps = [up("a"), up("b"), up("c")];
        assert_eq!(
            repair_decision(&cps, 10_000, AFTER, &spares(&["s1"]), false),
            RepairDecision::None
        );
    }

    #[test]
    fn recent_failure_waits() {
        let cps = [up("a"), up("b"), down("c", 9_800)];
        assert_eq!(
            repair_decision(&cps, 10_000, AFTER, &spares(&["s1"]), false),
            RepairDecision::None
        );
    }

    #[test]
    fn dead_control_plane_is_replaced_from_spares() {
        let cps = [up("a"), up("b"), down("c", 9_000)];
        assert_eq!(
            repair_decision(&cps, 10_000, AFTER, &spares(&["s1", "s2"]), false),
            RepairDecision::Replace {
                old: "c".to_string(),
                new_spare: "s1".to_string()
            }
        );
    }

    #[test]
    fn threshold_is_inclusive() {
        let cps = [up("a"), up("b"), down("c", 10_000 - AFTER)];
        assert!(matches!(
            repair_decision(&cps, 10_000, AFTER, &spares(&["s1"]), false),
            RepairDecision::Replace { .. }
        ));
    }

    #[test]
    fn empty_spare_pool_alerts() {
        let cps = [up("a"), up("b"), down("c", 9_000)];
        match repair_decision(&cps, 10_000, AFTER, &[], false) {
            RepairDecision::Alert { node_id, reason } => {
                assert_eq!(node_id, "c");
                assert!(reason.contains("spare"));
            }
            other => panic!("expected alert, got {other:?}"),
        }
    }

    #[test]
    fn spare_already_a_control_plane_is_skipped() {
        let cps = [up("a"), up("b"), down("c", 9_000)];
        assert_eq!(
            repair_decision(&cps, 10_000, AFTER, &spares(&["a", "s2"]), false),
            RepairDecision::Replace {
                old: "c".to_string(),
                new_spare: "s2".to_string()
            }
        );
    }

    #[test]
    fn busy_does_nothing() {
        let cps = [up("a"), up("b"), down("c", 1_000)];
        assert_eq!(
            repair_decision(&cps, 10_000, AFTER, &spares(&["s1"]), true),
            RepairDecision::None
        );
    }

    #[test]
    fn lost_quorum_alerts_and_never_replaces() {
        let cps = [up("a"), down("b", 1_000), down("c", 2_000)];
        match repair_decision(&cps, 10_000, AFTER, &spares(&["s1"]), false) {
            RepairDecision::Alert { node_id, reason } => {
                assert_eq!(node_id, "b");
                assert!(reason.contains("quorum"));
            }
            other => panic!("expected alert, got {other:?}"),
        }
    }

    #[test]
    fn single_control_plane_down_alerts() {
        let cps = [down("a", 1_000)];
        assert!(matches!(
            repair_decision(&cps, 10_000, AFTER, &spares(&["s1"]), false),
            RepairDecision::Alert { .. }
        ));
    }

    #[test]
    fn two_down_with_quorum_alerts() {
        let cps = [
            up("a"),
            up("b"),
            up("c"),
            down("d", 1_000),
            down("e", 9_900),
        ];
        match repair_decision(&cps, 10_000, AFTER, &spares(&["s1"]), false) {
            RepairDecision::Alert { node_id, reason } => {
                assert_eq!(node_id, "d");
                assert!(reason.contains("one at a time"));
            }
            other => panic!("expected alert, got {other:?}"),
        }
    }

    #[test]
    fn unhealthy_without_timestamp_is_not_dead() {
        let cps = [
            up("a"),
            up("b"),
            CpHealth {
                node_id: "c".to_string(),
                healthy: false,
                unhealthy_since: None,
            },
        ];
        assert_eq!(
            repair_decision(&cps, 10_000, AFTER, &spares(&["s1"]), false),
            RepairDecision::None
        );
    }
}
