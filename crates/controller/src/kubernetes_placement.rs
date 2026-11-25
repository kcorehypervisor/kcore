//! Placement checks for Kubernetes control planes.
//!
//! Pure rules over node ids and data centre ids. The gRPC plan and create
//! paths gather the inputs and reject on `Error`; `Warning` goes into the plan.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FindingLevel {
    Error,
    Warning,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlacementFinding {
    pub level: FindingLevel,
    pub message: String,
}

impl PlacementFinding {
    fn error(message: String) -> Self {
        Self {
            level: FindingLevel::Error,
            message,
        }
    }

    fn warning(message: String) -> Self {
        Self {
            level: FindingLevel::Warning,
            message,
        }
    }
}

/// Minimum control planes for an HA cluster: etcd needs a majority of three.
pub const HA_MIN_CONTROL_PLANES: usize = 3;

/// `control_planes` is `(node_id, dc_id)` per control plane. `controller_host`
/// and `ceph_mon_hosts` are node ids.
pub fn placement_findings(
    control_planes: &[(String, String)],
    ha_enabled: bool,
    other_dcs_exist: bool,
    controller_host: Option<&str>,
    ceph_mon_hosts: &[String],
) -> Vec<PlacementFinding> {
    let mut findings = Vec::new();

    if ha_enabled && control_planes.len() < HA_MIN_CONTROL_PLANES {
        findings.push(PlacementFinding::error(format!(
            "high availability needs at least {HA_MIN_CONTROL_PLANES} control planes, got {}",
            control_planes.len()
        )));
    }

    if let Some((_, first_dc)) = control_planes.first() {
        let one_dc = control_planes.iter().all(|(_, dc)| dc == first_dc);
        if one_dc && other_dcs_exist {
            findings.push(PlacementFinding::warning(format!(
                "all control planes are in data centre {first_dc}; losing it loses the API"
            )));
        }
    }

    if let Some(host) = controller_host.filter(|h| !h.is_empty()) {
        if control_planes.iter().any(|(node, _)| node == host) {
            findings.push(PlacementFinding::warning(format!(
                "control plane {host} also runs the kcore controller"
            )));
        }
    }

    for (node, _) in control_planes {
        if ceph_mon_hosts.iter().any(|mon| mon == node) {
            findings.push(PlacementFinding::warning(format!(
                "control plane {node} is also a Ceph monitor"
            )));
        }
    }

    findings
}

pub fn has_errors(findings: &[PlacementFinding]) -> bool {
    findings.iter().any(|f| f.level == FindingLevel::Error)
}

pub fn messages(findings: &[PlacementFinding], level: FindingLevel) -> Vec<String> {
    findings
        .iter()
        .filter(|f| f.level == level)
        .map(|f| f.message.clone())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cps(items: &[(&str, &str)]) -> Vec<(String, String)> {
        items
            .iter()
            .map(|(n, d)| (n.to_string(), d.to_string()))
            .collect()
    }

    fn levels(findings: &[PlacementFinding]) -> Vec<FindingLevel> {
        findings.iter().map(|f| f.level).collect()
    }

    #[test]
    fn ha_with_fewer_than_three_is_an_error() {
        let found = placement_findings(&cps(&[("a", "dc1"), ("b", "dc2")]), true, false, None, &[]);
        assert_eq!(levels(&found), vec![FindingLevel::Error]);
        assert!(has_errors(&found));
    }

    #[test]
    fn ha_with_three_is_clean() {
        let found = placement_findings(
            &cps(&[("a", "dc1"), ("b", "dc2"), ("c", "dc3")]),
            true,
            true,
            None,
            &[],
        );
        assert!(found.is_empty());
    }

    #[test]
    fn single_control_plane_without_ha_is_clean() {
        let found = placement_findings(&cps(&[("a", "dc1")]), false, false, None, &[]);
        assert!(found.is_empty());
    }

    #[test]
    fn one_dc_with_other_dcs_warns() {
        let found = placement_findings(
            &cps(&[("a", "dc1"), ("b", "dc1"), ("c", "dc1")]),
            true,
            true,
            None,
            &[],
        );
        assert_eq!(levels(&found), vec![FindingLevel::Warning]);
        assert!(found[0].message.contains("dc1"));
    }

    #[test]
    fn one_dc_without_other_dcs_is_clean() {
        let found = placement_findings(
            &cps(&[("a", "dc1"), ("b", "dc1"), ("c", "dc1")]),
            true,
            false,
            None,
            &[],
        );
        assert!(found.is_empty());
    }

    #[test]
    fn controller_host_warns() {
        let found = placement_findings(&cps(&[("a", "dc1")]), false, false, Some("a"), &[]);
        assert_eq!(levels(&found), vec![FindingLevel::Warning]);
        assert!(found[0].message.contains("controller"));
    }

    #[test]
    fn controller_on_another_node_is_clean() {
        let found = placement_findings(&cps(&[("a", "dc1")]), false, false, Some("z"), &[]);
        assert!(found.is_empty());
    }

    #[test]
    fn empty_controller_host_is_ignored() {
        let found = placement_findings(&cps(&[("a", "dc1")]), false, false, Some(""), &[]);
        assert!(found.is_empty());
    }

    #[test]
    fn ceph_mon_host_warns_once_per_node() {
        let found = placement_findings(
            &cps(&[("a", "dc1"), ("b", "dc1")]),
            false,
            false,
            None,
            &["b".to_string(), "x".to_string()],
        );
        assert_eq!(levels(&found), vec![FindingLevel::Warning]);
        assert!(found[0].message.contains('b'));
    }

    #[test]
    fn errors_and_warnings_combine() {
        let found = placement_findings(
            &cps(&[("a", "dc1")]),
            true,
            true,
            Some("a"),
            &["a".to_string()],
        );
        assert!(has_errors(&found));
        assert_eq!(messages(&found, FindingLevel::Error).len(), 1);
        assert_eq!(messages(&found, FindingLevel::Warning).len(), 3);
    }

    #[test]
    fn no_control_planes_has_no_dc_warning() {
        let found = placement_findings(&[], false, true, None, &[]);
        assert!(found.is_empty());
    }
}
