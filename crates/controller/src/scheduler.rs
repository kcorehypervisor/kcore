use crate::db::NodeRow;

/// Pick the ready node with the most free memory after accounting for
/// allocated VMs. Falls back to first-ready when no allocation data
/// is available.
pub fn select_node(nodes: &[NodeRow]) -> Option<&NodeRow> {
    nodes
        .iter()
        .filter(|n| n.status == "ready" && n.approval_status == "approved")
        .max_by_key(|n| free_capacity(n, Overcommit::default()))
}

/// Pick the ready node that can fit the requested resources and has the
/// most remaining capacity afterwards.
pub fn select_node_for_vm(
    nodes: &[NodeRow],
    requested_cpu: i32,
    requested_memory: i64,
) -> Option<&NodeRow> {
    select_with_constraints(
        nodes,
        requested_cpu,
        requested_memory,
        &Constraints::capacity_only(Overcommit::default()),
    )
}

pub fn select_node_for_vm_in_dc<'a>(
    nodes: &'a [NodeRow],
    requested_cpu: i32,
    requested_memory: i64,
    dc_id: &str,
) -> Option<&'a NodeRow> {
    let mut constraints = Constraints::capacity_only(Overcommit::default());
    constraints.dc = dc_id;
    select_with_constraints(nodes, requested_cpu, requested_memory, &constraints)
}

/// Live capacity ceiling. `1.0` is physical capacity. Heartbeat load is the
/// used side, so a ratio above 1 lets a quiet node take more VMs than its
/// core count while a loaded node stays ineligible.
#[derive(Debug, Clone, Copy)]
pub struct Overcommit {
    pub cpu: f64,
    pub memory: f64,
}

impl Default for Overcommit {
    fn default() -> Self {
        Self {
            cpu: 1.0,
            memory: 1.0,
        }
    }
}

pub struct Constraints<'a> {
    pub overcommit: Overcommit,
    pub dc: &'a str,
    pub required_labels: &'a [String],
    pub node_labels: &'a std::collections::HashMap<String, Vec<String>>,
    pub anti_affinity: &'a str,
    pub groups_on_node: &'a std::collections::HashMap<String, Vec<String>>,
}

impl<'a> Constraints<'a> {
    pub fn capacity_only(overcommit: Overcommit) -> Self {
        Self {
            overcommit,
            dc: "",
            required_labels: &[],
            node_labels: &EMPTY_LABELS,
            anti_affinity: "",
            groups_on_node: &EMPTY_GROUPS,
        }
    }
}

static EMPTY_LABELS: std::sync::LazyLock<std::collections::HashMap<String, Vec<String>>> =
    std::sync::LazyLock::new(std::collections::HashMap::new);
static EMPTY_GROUPS: std::sync::LazyLock<std::collections::HashMap<String, Vec<String>>> =
    std::sync::LazyLock::new(std::collections::HashMap::new);

pub fn schedulable_cpu(cores: i32, ratio: f64) -> i64 {
    scale_capacity(i64::from(cores), ratio)
}

pub fn schedulable_memory(bytes: i64, ratio: f64) -> i64 {
    scale_capacity(bytes, ratio)
}

fn scale_capacity(capacity: i64, ratio: f64) -> i64 {
    if capacity <= 0 || !ratio.is_finite() || ratio <= 0.0 {
        return 0;
    }
    let scaled = (capacity as f64) * ratio;
    if !scaled.is_finite() || scaled <= 0.0 {
        return 0;
    }
    scaled.floor() as i64
}

pub fn free_capacity(node: &NodeRow, overcommit: Overcommit) -> (i64, i64) {
    (
        schedulable_memory(node.memory_bytes, overcommit.memory) - node.memory_used,
        schedulable_cpu(node.cpu_cores, overcommit.cpu) - i64::from(node.cpu_used),
    )
}

pub fn node_has_capacity(
    node: &NodeRow,
    requested_cpu: i32,
    requested_memory: i64,
    overcommit: Overcommit,
) -> bool {
    let (free_mem, free_cpu) = free_capacity(node, overcommit);
    free_cpu >= i64::from(requested_cpu) && free_mem >= requested_memory
}

pub fn select_with_constraints<'a>(
    nodes: &'a [NodeRow],
    requested_cpu: i32,
    requested_memory: i64,
    constraints: &Constraints<'_>,
) -> Option<&'a NodeRow> {
    nodes
        .iter()
        .filter(|node| {
            node.status == "ready"
                && node.approval_status == "approved"
                && (constraints.dc.is_empty() || node.dc_id == constraints.dc)
                && node_has_capacity(
                    node,
                    requested_cpu,
                    requested_memory,
                    constraints.overcommit,
                )
                && labels_match(node, constraints)
                && anti_affinity_ok(node, constraints)
        })
        .max_by_key(|node| free_capacity(node, constraints.overcommit))
}

fn labels_match(node: &NodeRow, constraints: &Constraints<'_>) -> bool {
    if constraints.required_labels.is_empty() {
        return true;
    }
    let have = constraints
        .node_labels
        .get(&node.id)
        .map(Vec::as_slice)
        .unwrap_or(&[]);
    constraints
        .required_labels
        .iter()
        .all(|want| have.iter().any(|got| got == want))
}

pub fn normalize_label(raw: &str) -> Result<String, String> {
    let raw = raw.trim();
    let Some((key, value)) = raw.split_once('=') else {
        return Err(format!(
            "node label '{raw}' must be key=value, for example dc=dc-a"
        ));
    };
    if key.is_empty()
        || value.is_empty()
        || key.chars().any(char::is_whitespace)
        || value.chars().any(char::is_whitespace)
    {
        return Err(format!(
            "node label '{raw}' must be key=value with no spaces"
        ));
    }
    Ok(format!("{key}={value}"))
}

pub fn normalize_anti_affinity(raw: &str) -> Result<String, String> {
    let raw = raw.trim();
    if raw.is_empty() {
        return Ok(String::new());
    }
    let ok = raw.len() <= 64
        && raw
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.');
    if ok {
        Ok(raw.to_string())
    } else {
        Err(format!(
            "anti-affinity group '{raw}' must be 1-64 characters of letters, digits, '.', '_' or '-'"
        ))
    }
}

fn anti_affinity_ok(node: &NodeRow, constraints: &Constraints<'_>) -> bool {
    let group = constraints.anti_affinity.trim();
    if group.is_empty() {
        return true;
    }
    constraints
        .groups_on_node
        .get(&node.id)
        .map(|groups| !groups.iter().any(|existing| existing == group))
        .unwrap_or(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn node(id: &str, cpu: i32, mem: i64, cpu_used: i32, mem_used: i64) -> NodeRow {
        NodeRow {
            id: id.into(),
            hostname: id.into(),
            address: format!("{id}:9091"),
            cpu_cores: cpu,
            memory_bytes: mem,
            status: "ready".into(),
            last_heartbeat: String::new(),
            gateway_interface: String::new(),
            cpu_used,
            memory_used: mem_used,
            storage_backend: "filesystem".into(),
            disable_vxlan: false,
            approval_status: "approved".into(),
            cert_expiry_days: -1,
            luks_method: String::new(),
            dc_id: "DC1".into(),
        }
    }

    #[test]
    fn selects_node_with_most_free_memory() {
        let nodes = vec![
            node("n1", 8, 16_000_000_000, 4, 12_000_000_000),
            node("n2", 8, 16_000_000_000, 2, 4_000_000_000),
        ];
        let picked = select_node(&nodes).unwrap();
        assert_eq!(picked.id, "n2");
    }

    #[test]
    fn select_node_for_vm_respects_capacity() {
        let nodes = vec![
            node("n1", 4, 8_000_000_000, 3, 6_000_000_000),
            node("n2", 8, 16_000_000_000, 2, 4_000_000_000),
        ];
        let picked = select_node_for_vm(&nodes, 4, 8_000_000_000).unwrap();
        assert_eq!(picked.id, "n2");
    }

    #[test]
    fn select_node_for_vm_returns_none_when_no_fit() {
        let nodes = vec![node("n1", 4, 8_000_000_000, 4, 8_000_000_000)];
        assert!(select_node_for_vm(&nodes, 2, 4_000_000_000).is_none());
    }

    #[test]
    fn skips_non_ready_nodes() {
        let mut n = node("n1", 8, 16_000_000_000, 0, 0);
        n.status = "unknown".into();
        assert!(select_node(&[n]).is_none());
    }

    #[test]
    fn select_node_for_vm_in_dc_filters_by_dc() {
        let mut n1 = node("n1", 8, 16_000_000_000, 0, 0);
        n1.dc_id = "DC1".into();
        let mut n2 = node("n2", 8, 16_000_000_000, 0, 0);
        n2.dc_id = "DC2".into();
        let nodes = vec![n1, n2];

        let picked = select_node_for_vm_in_dc(&nodes, 2, 4_000_000_000, "DC2").unwrap();
        assert_eq!(picked.id, "n2");
    }

    #[test]
    fn select_node_for_vm_in_dc_returns_none_for_empty_dc() {
        let nodes = vec![node("n1", 8, 16_000_000_000, 0, 0)];
        assert!(select_node_for_vm_in_dc(&nodes, 2, 4_000_000_000, "DC2").is_none());
    }

    #[test]
    fn select_node_for_vm_in_dc_picks_best_in_dc() {
        let mut n1 = node("n1", 8, 16_000_000_000, 6, 12_000_000_000);
        n1.dc_id = "DC1".into();
        let mut n2 = node("n2", 8, 16_000_000_000, 2, 4_000_000_000);
        n2.dc_id = "DC1".into();
        let mut n3 = node("n3", 8, 32_000_000_000, 0, 0);
        n3.dc_id = "DC2".into();
        let nodes = vec![n1, n2, n3];

        let picked = select_node_for_vm_in_dc(&nodes, 2, 4_000_000_000, "DC1").unwrap();
        assert_eq!(picked.id, "n2");
    }

    #[test]
    fn label_affinity_skips_nodes_missing_the_label() {
        let nodes = vec![
            node("n1", 8, 16_000_000_000, 0, 0),
            node("n2", 8, 16_000_000_000, 0, 0),
        ];
        let mut labels = std::collections::HashMap::new();
        labels.insert("n2".to_string(), vec!["dc=dc-a".to_string()]);
        let required = vec!["dc=dc-a".to_string()];
        let mut constraints = Constraints::capacity_only(Overcommit::default());
        constraints.required_labels = &required;
        constraints.node_labels = &labels;
        let picked = select_with_constraints(&nodes, 2, 1_000, &constraints).unwrap();
        assert_eq!(picked.id, "n2");
    }

    #[test]
    fn anti_affinity_keeps_peers_on_different_nodes() {
        let nodes = vec![
            node("n1", 8, 16_000_000_000, 0, 0),
            node("n2", 8, 16_000_000_000, 1, 1_000),
        ];
        let mut groups = std::collections::HashMap::new();
        groups.insert("n1".to_string(), vec!["web".to_string()]);
        let mut constraints = Constraints::capacity_only(Overcommit::default());
        constraints.anti_affinity = "web";
        constraints.groups_on_node = &groups;
        let picked = select_with_constraints(&nodes, 2, 1_000, &constraints).unwrap();
        assert_eq!(picked.id, "n2");
    }

    #[test]
    fn anti_affinity_returns_none_when_every_node_already_has_the_group() {
        let nodes = vec![node("n1", 8, 16_000_000_000, 0, 0)];
        let mut groups = std::collections::HashMap::new();
        groups.insert("n1".to_string(), vec!["web".to_string()]);
        let mut constraints = Constraints::capacity_only(Overcommit::default());
        constraints.anti_affinity = "web";
        constraints.groups_on_node = &groups;
        assert!(select_with_constraints(&nodes, 1, 1, &constraints).is_none());
    }

    #[test]
    fn overcommit_admits_a_vm_past_physical_cores_when_load_is_low() {
        let nodes = vec![node("n1", 4, 8_000_000_000, 1, 1_000_000_000)];
        assert!(select_node_for_vm(&nodes, 4, 1_000).is_none());
        let mut constraints = Constraints::capacity_only(Overcommit {
            cpu: 2.0,
            memory: 1.0,
        });
        constraints.overcommit.memory = 1.0;
        let picked = select_with_constraints(&nodes, 4, 1_000, &constraints).unwrap();
        assert_eq!(picked.id, "n1");
    }
}

/// Property-based tests (Phase 2) — node placement.
///
/// `select_node_for_vm` is invoked on every VM create/migrate; the
/// strongest guarantees we want are: (a) it never picks an ineligible
/// node (wrong status / approval / DC, insufficient capacity), and (b)
/// the picked node is **maximal** w.r.t. the documented `(free_mem,
/// free_cpu)` ordering among the eligible set.
#[cfg(test)]
mod proptests {
    use super::*;
    use proptest::prelude::*;

    fn arb_node() -> impl Strategy<Value = NodeRow> {
        (
            "[a-z][a-z0-9-]{0,7}", // id
            1i32..=64,             // cpu_cores
            (1i64..=(1i64 << 38)), // memory_bytes
            0i32..=64,             // cpu_used (clamped below)
            0i64..=(1i64 << 38),   // memory_used (clamped below)
            prop::sample::select(vec!["ready", "unknown", "draining"]),
            prop::sample::select(vec!["approved", "pending", "rejected"]),
            prop::sample::select(vec!["DC1", "DC2", "DC3"]),
        )
            .prop_map(
                |(id, cpu, mem, cpu_used, mem_used, status, approval, dc)| NodeRow {
                    id: id.clone(),
                    hostname: id.clone(),
                    address: format!("{id}:9091"),
                    cpu_cores: cpu,
                    memory_bytes: mem,
                    status: status.into(),
                    last_heartbeat: String::new(),
                    gateway_interface: String::new(),
                    cpu_used: cpu_used.min(cpu),
                    memory_used: mem_used.min(mem),
                    storage_backend: "filesystem".into(),
                    disable_vxlan: false,
                    approval_status: approval.into(),
                    cert_expiry_days: -1,
                    luks_method: String::new(),
                    dc_id: dc.into(),
                },
            )
    }

    proptest! {
        #![proptest_config(ProptestConfig {
            cases: 1_000,
            .. ProptestConfig::default()
        })]

        /// `select_node` only ever returns `ready` + `approved` nodes.
        #[test]
        fn select_node_only_returns_eligible_nodes(
            nodes in proptest::collection::vec(arb_node(), 0..10),
        ) {
            if let Some(picked) = select_node(&nodes) {
                prop_assert_eq!(picked.status.as_str(), "ready");
                prop_assert_eq!(picked.approval_status.as_str(), "approved");
            }
        }

        /// `select_node_for_vm` never returns a node whose remaining
        /// capacity is less than what was requested.
        #[test]
        fn select_node_for_vm_respects_capacity(
            nodes in proptest::collection::vec(arb_node(), 0..10),
            requested_cpu in 1i32..=32,
            requested_mem in 1i64..=(1i64 << 36),
        ) {
            if let Some(picked) = select_node_for_vm(&nodes, requested_cpu, requested_mem) {
                prop_assert!(picked.status == "ready");
                prop_assert!(picked.approval_status == "approved");
                prop_assert!((picked.cpu_cores - picked.cpu_used) >= requested_cpu);
                prop_assert!((picked.memory_bytes - picked.memory_used) >= requested_mem);
            }
        }

        /// **Maximality**: the picked node has the largest `(free_mem,
        /// free_cpu)` tuple among eligible nodes. No eligible node may
        /// have a strictly larger tuple than the winner.
        #[test]
        fn select_node_for_vm_picks_max_remaining_capacity(
            nodes in proptest::collection::vec(arb_node(), 0..10),
            requested_cpu in 1i32..=8,
            requested_mem in 1i64..=(1i64 << 32),
        ) {
            let picked = select_node_for_vm(&nodes, requested_cpu, requested_mem);
            let eligible: Vec<&NodeRow> = nodes
                .iter()
                .filter(|n| {
                    n.status == "ready"
                        && n.approval_status == "approved"
                        && (n.cpu_cores - n.cpu_used) >= requested_cpu
                        && (n.memory_bytes - n.memory_used) >= requested_mem
                })
                .collect();
            prop_assert_eq!(picked.is_some(), !eligible.is_empty());
            if let Some(p) = picked {
                let p_key = (p.memory_bytes - p.memory_used, (p.cpu_cores - p.cpu_used) as i64);
                for n in &eligible {
                    let k = (n.memory_bytes - n.memory_used, (n.cpu_cores - n.cpu_used) as i64);
                    prop_assert!(k <= p_key, "eligible node {} has larger key {:?} than winner {:?}", n.id, k, p_key);
                }
            }
        }

        /// `select_node_for_vm_in_dc` is `select_node_for_vm` after
        /// filtering by `dc_id`. We assert the filter equivalence.
        #[test]
        fn select_node_for_vm_in_dc_equivalent_to_dc_prefilter(
            nodes in proptest::collection::vec(arb_node(), 0..10),
            requested_cpu in 1i32..=8,
            requested_mem in 1i64..=(1i64 << 32),
            dc in prop::sample::select(vec!["DC1", "DC2", "DC3"]),
        ) {
            let dc_only: Vec<NodeRow> =
                nodes.iter().filter(|n| n.dc_id == dc).cloned().collect();
            let from_helper =
                select_node_for_vm_in_dc(&nodes, requested_cpu, requested_mem, dc).map(|n| n.id.clone());
            let from_prefilter = select_node_for_vm(&dc_only, requested_cpu, requested_mem)
                .map(|n| n.id.clone());
            prop_assert_eq!(from_helper, from_prefilter);
        }
    }
}
