//! Capacity-aware VM placement.
//!
//! The controller picks the ready node with the most free memory, then the
//! most free CPU, then the lowest id. Free capacity is physical capacity
//! times the overcommit percentage, minus the live heartbeat load. Optional
//! label affinity and anti-affinity are hard filters.

/// A node as the scheduler sees it. Resource units are abstract so the same
/// comparison works for MiB and millicores.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SchedNode {
    pub id: u8,
    pub ready: bool,
    /// Physical memory capacity.
    pub memory: u8,
    /// Physical CPU capacity.
    pub cpu: u8,
    pub used_memory: u8,
    pub used_cpu: u8,
    /// Placement label. `0` means the node carries no label.
    pub label: u8,
    /// Whether the node already hosts a VM in the requested anti-affinity group.
    pub hosts_anti_affinity_group: bool,
}

/// A VM the controller wants to place.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PlacementRequest {
    pub memory: u8,
    pub cpu: u8,
    /// Required node label. `0` means no affinity constraint.
    pub required_label: u8,
    /// When set, skip nodes that already host the anti-affinity group.
    pub anti_affinity: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScheduleError {
    EmptyCluster,
    NoReadyNode,
    InsufficientCapacity,
    AffinityUnsatisfied,
    AntiAffinityUnsatisfied,
}

/// Free units after applying `overcommit_pct` (100 = no overcommit).
pub fn free_capacity(capacity: u8, used: u8, overcommit_pct: u8) -> u16 {
    let scaled = u16::from(capacity).saturating_mul(u16::from(overcommit_pct)) / 100;
    scaled.saturating_sub(u16::from(used))
}

fn has_capacity(node: &SchedNode, request: &PlacementRequest, overcommit_pct: u8) -> bool {
    free_capacity(node.memory, node.used_memory, overcommit_pct) >= u16::from(request.memory)
        && free_capacity(node.cpu, node.used_cpu, overcommit_pct) >= u16::from(request.cpu)
}

fn matches_affinity(node: &SchedNode, request: &PlacementRequest) -> bool {
    request.required_label == 0 || node.label == request.required_label
}

fn matches_anti_affinity(node: &SchedNode, request: &PlacementRequest) -> bool {
    !(request.anti_affinity && node.hosts_anti_affinity_group)
}

/// A node the scheduler is allowed to return for this request.
pub fn node_is_eligible(node: &SchedNode, request: &PlacementRequest, overcommit_pct: u8) -> bool {
    node.ready
        && has_capacity(node, request, overcommit_pct)
        && matches_affinity(node, request)
        && matches_anti_affinity(node, request)
}

fn preferred<'a>(
    candidate: &'a SchedNode,
    current: &'a SchedNode,
    overcommit_pct: u8,
) -> &'a SchedNode {
    let candidate_mem = free_capacity(candidate.memory, candidate.used_memory, overcommit_pct);
    let current_mem = free_capacity(current.memory, current.used_memory, overcommit_pct);
    if candidate_mem != current_mem {
        return if candidate_mem > current_mem {
            candidate
        } else {
            current
        };
    }
    let candidate_cpu = free_capacity(candidate.cpu, candidate.used_cpu, overcommit_pct);
    let current_cpu = free_capacity(current.cpu, current.used_cpu, overcommit_pct);
    if candidate_cpu != current_cpu {
        return if candidate_cpu > current_cpu {
            candidate
        } else {
            current
        };
    }
    if candidate.id < current.id {
        candidate
    } else {
        current
    }
}

/// Choose a node for `request`, or explain why no node qualifies.
pub fn select_node(
    nodes: &[SchedNode],
    request: &PlacementRequest,
    overcommit_pct: u8,
) -> Result<u8, ScheduleError> {
    if nodes.is_empty() {
        return Err(ScheduleError::EmptyCluster);
    }

    let mut best: Option<&SchedNode> = None;
    let mut any_ready = false;
    let mut any_capacity = false;
    let mut any_affinity = false;

    for node in nodes {
        if !node.ready {
            continue;
        }
        any_ready = true;
        if !has_capacity(node, request, overcommit_pct) {
            continue;
        }
        any_capacity = true;
        if !matches_affinity(node, request) {
            continue;
        }
        any_affinity = true;
        if !matches_anti_affinity(node, request) {
            continue;
        }
        best = Some(match best {
            None => node,
            Some(current) => preferred(node, current, overcommit_pct),
        });
    }

    if let Some(node) = best {
        return Ok(node.id);
    }
    if !any_ready {
        return Err(ScheduleError::NoReadyNode);
    }
    if !any_capacity {
        return Err(ScheduleError::InsufficientCapacity);
    }
    if request.required_label != 0 && !any_affinity {
        return Err(ScheduleError::AffinityUnsatisfied);
    }
    Err(ScheduleError::AntiAffinityUnsatisfied)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ready(id: u8, memory: u8, used_memory: u8, cpu: u8, used_cpu: u8) -> SchedNode {
        SchedNode {
            id,
            ready: true,
            memory,
            cpu,
            used_memory,
            used_cpu,
            label: 0,
            hosts_anti_affinity_group: false,
        }
    }

    fn tagged(mut node: SchedNode, label: u8, anti: bool) -> SchedNode {
        node.label = label;
        node.hosts_anti_affinity_group = anti;
        node
    }

    fn request(memory: u8, cpu: u8, label: u8, anti: bool) -> PlacementRequest {
        PlacementRequest {
            memory,
            cpu,
            required_label: label,
            anti_affinity: anti,
        }
    }

    #[test]
    fn picks_most_free_memory_then_cpu_then_lowest_id() {
        let nodes = [
            ready(2, 100, 40, 100, 10),
            ready(1, 100, 40, 100, 50),
            ready(3, 100, 10, 10, 0),
        ];
        let choice = select_node(&nodes, &request(10, 10, 0, false), 100).unwrap();
        assert_eq!(choice, 3);
    }

    #[test]
    fn tie_on_memory_uses_cpu_then_id() {
        let nodes = [
            ready(5, 80, 0, 40, 10),
            ready(4, 80, 0, 40, 10),
            ready(6, 80, 0, 90, 0),
        ];
        let choice = select_node(&nodes, &request(1, 1, 0, false), 100).unwrap();
        assert_eq!(choice, 6);

        let tied = [ready(5, 80, 0, 40, 0), ready(2, 80, 0, 40, 0)];
        let choice = select_node(&tied, &request(1, 1, 0, false), 100).unwrap();
        assert_eq!(choice, 2);
    }

    #[test]
    fn rejects_not_ready_and_reports_why() {
        let mut offline = ready(1, 100, 0, 100, 0);
        offline.ready = false;
        let nodes = [offline];
        assert_eq!(
            select_node(&nodes, &request(1, 1, 0, false), 100),
            Err(ScheduleError::NoReadyNode)
        );
    }

    #[test]
    fn overcommit_allows_sum_past_physical_capacity() {
        let nodes = [ready(1, 10, 8, 10, 8)];
        assert!(select_node(&nodes, &request(10, 10, 0, false), 100).is_err());
        assert_eq!(select_node(&nodes, &request(10, 10, 0, false), 200), Ok(1));
    }

    #[test]
    fn affinity_and_anti_affinity_are_hard_filters() {
        let nodes = [
            tagged(ready(1, 100, 0, 100, 0), 7, false),
            tagged(ready(2, 100, 0, 100, 0), 9, true),
        ];
        assert_eq!(select_node(&nodes, &request(1, 1, 9, false), 100), Ok(2));
        assert_eq!(
            select_node(&nodes, &request(1, 1, 9, true), 100),
            Err(ScheduleError::AntiAffinityUnsatisfied)
        );
        assert_eq!(
            select_node(&nodes, &request(1, 1, 3, false), 100),
            Err(ScheduleError::AffinityUnsatisfied)
        );
    }

    #[test]
    fn empty_cluster() {
        assert_eq!(
            select_node(&[], &request(1, 1, 0, false), 100),
            Err(ScheduleError::EmptyCluster)
        );
    }
}
