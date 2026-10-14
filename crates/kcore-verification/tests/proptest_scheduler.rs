use kcore_verification::scheduler::*;
use proptest::prelude::*;

fn arb_nodes() -> impl Strategy<Value = Vec<SchedNode>> {
    prop::collection::vec(
        (
            prop::bool::ANY,
            any::<u8>(),
            any::<u8>(),
            any::<u8>(),
            any::<u8>(),
            any::<u8>(),
            prop::bool::ANY,
        ),
        1..=4,
    )
    .prop_map(|rows| {
        rows.into_iter()
            .enumerate()
            .map(
                |(index, (ready, memory, cpu, used_memory, used_cpu, label, anti))| SchedNode {
                    id: (index as u8) + 1,
                    ready,
                    memory,
                    cpu,
                    used_memory,
                    used_cpu,
                    label,
                    hosts_anti_affinity_group: anti,
                },
            )
            .collect()
    })
}

fn arb_request() -> impl Strategy<Value = PlacementRequest> {
    (any::<u8>(), any::<u8>(), any::<u8>(), prop::bool::ANY).prop_map(
        |(memory, cpu, required_label, anti_affinity)| PlacementRequest {
            memory,
            cpu,
            required_label,
            anti_affinity,
        },
    )
}

proptest! {
    #[test]
    fn selected_node_is_eligible(
        nodes in arb_nodes(),
        request in arb_request(),
        overcommit in 100u8..=200u8
    ) {
        prop_assume!(!nodes.is_empty());
        if let Ok(id) = select_node(&nodes, &request, overcommit) {
            let chosen = nodes.iter().find(|node| node.id == id).unwrap();
            prop_assert!(node_is_eligible(chosen, &request, overcommit));
            for node in &nodes {
                if node_is_eligible(node, &request, overcommit) {
                    let chosen_mem = free_capacity(chosen.memory, chosen.used_memory, overcommit);
                    let other_mem = free_capacity(node.memory, node.used_memory, overcommit);
                    prop_assert!(chosen_mem >= other_mem);
                    if chosen_mem == other_mem {
                        let chosen_cpu = free_capacity(chosen.cpu, chosen.used_cpu, overcommit);
                        let other_cpu = free_capacity(node.cpu, node.used_cpu, overcommit);
                        prop_assert!(chosen_cpu >= other_cpu);
                        if chosen_cpu == other_cpu {
                            prop_assert!(chosen.id <= node.id);
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn selection_is_deterministic(
        nodes in arb_nodes(),
        request in arb_request(),
        overcommit in 0u8..=200u8
    ) {
        prop_assert_eq!(
            select_node(&nodes, &request, overcommit),
            select_node(&nodes, &request, overcommit)
        );
    }

    #[test]
    fn free_capacity_never_exceeds_scaled(
        capacity in any::<u8>(),
        used in any::<u8>(),
        overcommit in 0u8..=200u8
    ) {
        let free = free_capacity(capacity, used, overcommit);
        let scaled = u16::from(capacity).saturating_mul(u16::from(overcommit)) / 100;
        prop_assert!(free <= scaled);
        if used as u16 >= scaled {
            prop_assert_eq!(free, 0);
        }
    }

    #[test]
    fn no_ready_node_is_reported(overcommit in 100u8..=150u8, memory in any::<u8>(), cpu in any::<u8>()) {
        let nodes = [
            SchedNode {
                id: 1,
                ready: false,
                memory: 200,
                cpu: 200,
                used_memory: 0,
                used_cpu: 0,
                label: 0,
                hosts_anti_affinity_group: false,
            },
        ];
        let request = PlacementRequest {
            memory,
            cpu,
            required_label: 0,
            anti_affinity: false,
        };
        prop_assert_eq!(
            select_node(&nodes, &request, overcommit),
            Err(ScheduleError::NoReadyNode)
        );
    }
}

#[cfg(test)]
mod concrete {
    use super::*;

    #[test]
    fn insufficient_capacity_when_ready_but_full() {
        let nodes = [SchedNode {
            id: 1,
            ready: true,
            memory: 4,
            cpu: 4,
            used_memory: 4,
            used_cpu: 4,
            label: 0,
            hosts_anti_affinity_group: false,
        }];
        let request = PlacementRequest {
            memory: 5,
            cpu: 1,
            required_label: 0,
            anti_affinity: false,
        };
        assert_eq!(
            select_node(&nodes, &request, 100),
            Err(ScheduleError::InsufficientCapacity)
        );
    }
}
